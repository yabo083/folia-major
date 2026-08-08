// src-tauri/src/network.rs
// M5 网络层：歌词代理 / CORS 中继 / AI 主题生成 / 外链打开。
// 镜像 folia-major/electron/main.cjs 的 proxyLyricRequest / generate-theme IPC，
// 以及 shared/themeSanitizer.cjs 的 sanitizeDualTheme；在代理上补安全加固：
// 严格 URL 解析、host 白名单、方法/头/体校验、尺寸/时限、重定向逐跳白名单复核。

use crate::settings::SettingsStore;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::time::Duration;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use url::Url;

// ---------------------------------------------------------------------------
// Constants / policy
// ---------------------------------------------------------------------------

// Host 白名单：与 Electron isAllowedLyricProxyHost 完全一致（含大小写无关）。
pub(crate) fn is_allowed_lyric_proxy_host(hostname: &str) -> bool {
    let host = hostname.trim().to_ascii_lowercase();
    host == "qq.com"
        || host.ends_with(".qq.com")
        || host == "y.gtimg.cn"
        || host == "kugou.com"
        || host.ends_with(".kugou.com")
        || host == "kgimg.com"
        || host.ends_with(".kgimg.com")
        || host == "amll-ttml-db.stevexmh.net"
}

// amll 特例：404 → 204（与 Electron isAmllDbHost 一致）。
fn is_amll_db_host(hostname: &str) -> bool {
    hostname
        .trim()
        .eq_ignore_ascii_case("amll-ttml-db.stevexmh.net")
}

// 转发时剔除的头（同 Electron proxyLyricRequest 与 vite devLyricProxyPlugin）。
const IGNORED_FORWARD_HEADERS: &[&str] =
    &["host", "connection", "content-length", "origin", "referer"];

// 逐跳头：绝不回传给调用方（防中间人语义泄露 / 请求走私类头）。
const HOP_BY_HOP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

const MAX_BODY_BYTES: usize = 8 * 1024 * 1024; // 请求/响应体上限 8MB（歌词/封面足够）。
const MAX_HEADER_BYTES: usize = 64 * 1024; // 转发头总字节上限。
const MAX_HEADER_COUNT: usize = 64;
const MAX_REDIRECTS: usize = 5; // 手动重定向上限。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOTAL_DEADLINE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Shared state (managed by lib.rs; one reqwest client reused by the relay)
// ---------------------------------------------------------------------------

pub struct NetworkState {
    client: reqwest::Client,
}

impl NetworkState {
    pub fn new() -> Self {
        Self {
            client: build_lyric_proxy_client(),
        }
    }
}

impl Default for NetworkState {
    fn default() -> Self {
        Self::new()
    }
}

// 代理客户端：无自动重定向（手动逐跳复核），env 代理由 reqwest auto_sys_proxy 自动读取
// （HTTPS_PROXY/HTTP_PROXY/ALL_PROXY/NO_PROXY，含小写变体），即"系统代理检测"。
fn build_lyric_proxy_client() -> reqwest::Client {
    base_client_builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// 测试复用：保留 builder 以便注入 DNS 覆盖（本地集成测试把白名单域名映射到 127.0.0.1）。
fn base_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().redirect(Policy::none())
}

// ---------------------------------------------------------------------------
// Command input / output shapes (与 Electron IPC 契约一致)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct LyricProxyInit {
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<Map<String, Value>>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricProxyResponse {
    ok: bool,
    status: u16,
    status_text: String,
    headers: Map<String, Value>,
    body_text: String,
    // 原始响应字节的 base64：供 shim CORS 中继保留二进制保真（封面等），
    // 文本调用方仍读 bodyText（与 Electron 契约一致）。
    body_data: Option<String>,
}

// 内部错误：决定命令是"结构化 403 响应"还是"reject"。
#[derive(Debug)]
pub(crate) enum LyricProxyError {
    BadRequest(String),
    Forbidden(String),
    Transport(String),
}

// ---------------------------------------------------------------------------
// Validation helpers (pure, unit-testable)
// ---------------------------------------------------------------------------

fn is_http_method(method: &str) -> bool {
    ALLOWED_METHODS.contains(&method.to_ascii_uppercase().as_str())
}

// RFC 7230 tchar：合法 HeaderName 字符集。
fn is_header_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

// 把调用方传入的 header 值归一为字符串；null 跳过，非标量拒绝。
fn header_value_to_string(value: &Value) -> Result<Option<String>, String> {
    match value {
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        Value::Null => Ok(None),
        _ => Err(format!("invalid header value: {value}")),
    }
}

// 转发前清洗：剔除白名单忽略头 + 逐跳头，校验名称/值合法性与尺寸上限。
fn sanitize_forward_headers(
    headers: &Map<String, Value>,
) -> Result<Vec<(String, String)>, LyricProxyError> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut total_bytes = 0usize;
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if IGNORED_FORWARD_HEADERS.contains(&lower.as_str())
            || HOP_BY_HOP_RESPONSE_HEADERS.contains(&lower.as_str())
        {
            continue;
        }
        if !is_header_token(name) {
            return Err(LyricProxyError::BadRequest(format!(
                "invalid header name: {name:?}"
            )));
        }
        let Some(raw) = header_value_to_string(value).map_err(LyricProxyError::BadRequest)? else {
            continue;
        };
        if raw.contains('\r') || raw.contains('\n') {
            return Err(LyricProxyError::BadRequest(format!(
                "header {name:?} contains CR/LF"
            )));
        }
        total_bytes += lower.len() + raw.len();
        if total_bytes > MAX_HEADER_BYTES || out.len() >= MAX_HEADER_COUNT {
            return Err(LyricProxyError::BadRequest(format!(
                "forwarded headers exceed limit"
            )));
        }
        out.push((name.clone(), raw));
    }
    Ok(out)
}

// 相对/绝对 Location 解析 + scheme 校验（http/https only）。
fn resolve_redirect_url(current: &Url, location: &str) -> Result<Url, LyricProxyError> {
    let target = current
        .join(location)
        .map_err(|e| LyricProxyError::BadRequest(format!("invalid redirect location: {e}")))?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(LyricProxyError::Forbidden(format!(
            "redirect to non-http(s) target: {}",
            target.scheme()
        )));
    }
    Ok(target)
}

// 响应头归一：小写键，重复头用 ", " 合并（同 JS Headers.entries()），剔除逐跳头。
fn normalize_response_headers(headers: &reqwest::header::HeaderMap) -> Map<String, Value> {
    let mut out: Map<String, Value> = Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP_RESPONSE_HEADERS.contains(&key.as_str()) {
            continue;
        }
        let value = value.to_str().unwrap_or_default().to_string();
        if let Some(existing) = out.get_mut(&key) {
            if let Value::String(existing) = existing {
                existing.push_str(", ");
                existing.push_str(&value);
            }
        } else {
            out.insert(key, Value::String(value));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Core proxy execution
// ---------------------------------------------------------------------------

async fn execute_proxy_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: Url,
    headers: Vec<(String, String)>,
    body: Option<String>,
) -> Result<LyricProxyResponse, LyricProxyError> {
    let original_host = url
        .host_str()
        .ok_or_else(|| LyricProxyError::BadRequest("url has no host".to_string()))?
        .to_string();
    let mut current = url;
    let mut hops = 0usize;

    loop {
        let hostname = current
            .host_str()
            .ok_or_else(|| LyricProxyError::BadRequest("url has no host".to_string()))?;
        // 白名单复核：初始 URL 与每个重定向目标都必须命中，防止白名单逃逸。
        if !is_allowed_lyric_proxy_host(hostname) {
            return Err(LyricProxyError::Forbidden(format!(
                "Forbidden lyric proxy host: {hostname}"
            )));
        }

        let mut builder = client.request(method.clone(), current.clone());
        for (key, value) in &headers {
            builder = builder.header(key, value);
        }
        if let Some(body) = &body {
            let is_body_method = method == reqwest::Method::POST
                || method == reqwest::Method::PUT
                || method == reqwest::Method::PATCH;
            if is_body_method {
                builder = builder.body(body.clone());
            }
        }

        let response = builder
            .send()
            .await
            .map_err(|e| LyricProxyError::Transport(format!("lyric proxy request failed: {e}")))?;

        if response.status().is_redirection() {
            if hops >= MAX_REDIRECTS {
                return Err(LyricProxyError::Forbidden("Too many redirects".to_string()));
            }
            if let Some(location) = response.headers().get(LOCATION) {
                let location = location.to_str().map_err(|_| {
                    LyricProxyError::BadRequest("invalid redirect location header".to_string())
                })?;
                let next = resolve_redirect_url(&current, location)?;
                hops += 1;
                current = next;
                continue;
            }
        }

        return finalize_response(response, &original_host).await;
    }
}

async fn finalize_response(
    response: reqwest::Response,
    original_host: &str,
) -> Result<LyricProxyResponse, LyricProxyError> {
    let status = response.status();

    // 文档化特例：amll 域 404 → 204 空响应（同 Electron / vite dev 插件）。
    if is_amll_db_host(original_host) && status == reqwest::StatusCode::NOT_FOUND {
        return Ok(LyricProxyResponse {
            ok: true,
            status: 204,
            status_text: "No Content".to_string(),
            headers: Map::new(),
            body_text: String::new(),
            body_data: None,
        });
    }

    let headers = normalize_response_headers(response.headers());
    let bytes = response.bytes().await.map_err(|e| {
        LyricProxyError::Transport(format!("lyric proxy response body read failed: {e}"))
    })?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(LyricProxyError::BadRequest(format!(
            "response body exceeds {MAX_BODY_BYTES} bytes"
        )));
    }

    Ok(LyricProxyResponse {
        ok: status.is_success(),
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_string(),
        headers,
        body_text: String::from_utf8_lossy(&bytes).into_owned(),
        body_data: Some(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &bytes,
        )),
    })
}

pub(crate) async fn lyric_proxy_fetch_inner(
    client: &reqwest::Client,
    url: String,
    init: Option<LyricProxyInit>,
) -> Result<LyricProxyResponse, LyricProxyError> {
    let init = init.unwrap_or_default();

    let target_url =
        Url::parse(&url).map_err(|e| LyricProxyError::BadRequest(format!("invalid url: {e}")))?;
    if !matches!(target_url.scheme(), "http" | "https") {
        return Err(LyricProxyError::BadRequest(format!(
            "unsupported scheme: {}",
            target_url.scheme()
        )));
    }

    let method_str = init.method.as_deref().unwrap_or("GET");
    if !is_http_method(method_str) {
        return Err(LyricProxyError::BadRequest(format!(
            "unsupported method: {method_str}"
        )));
    }
    let method = reqwest::Method::from_bytes(method_str.to_ascii_uppercase().as_bytes())
        .map_err(|e| LyricProxyError::BadRequest(format!("invalid method: {e}")))?;

    let headers = sanitize_forward_headers(init.headers.as_ref().unwrap_or(&Map::new()))?;

    let body = match init.body {
        Some(body) if body.len() > MAX_BODY_BYTES => {
            return Err(LyricProxyError::BadRequest(format!(
                "request body exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        Some(body) if body.is_empty() => None,
        Some(body) => Some(body),
        None => None,
    };

    tokio::time::timeout(
        TOTAL_DEADLINE,
        execute_proxy_request(client, method, target_url, headers, body),
    )
    .await
    .map_err(|_| LyricProxyError::Transport("lyric proxy request timed out".to_string()))?
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

// 命令结果映射：BadRequest/Transport → reject；Forbidden → 结构化 403 响应。
fn into_command_result(
    result: Result<LyricProxyResponse, LyricProxyError>,
) -> Result<LyricProxyResponse, String> {
    match result {
        Ok(response) => Ok(response),
        Err(LyricProxyError::BadRequest(message)) | Err(LyricProxyError::Transport(message)) => {
            Err(message)
        }
        Err(LyricProxyError::Forbidden(message)) => Ok(LyricProxyResponse {
            ok: false,
            status: 403,
            status_text: "Forbidden".to_string(),
            headers: Map::new(),
            body_text: message,
            body_data: None,
        }),
    }
}

#[tauri::command]
// 歌词代理：白名单校验 + 转发 + amll 404→204；返回 { ok, status, statusText, headers, bodyText }。
pub async fn lyric_proxy_fetch(
    url: String,
    init: Option<LyricProxyInit>,
    state: State<'_, NetworkState>,
) -> Result<LyricProxyResponse, String> {
    into_command_result(lyric_proxy_fetch_inner(&state.client, url, init).await)
}

#[tauri::command]
// 外链打开：仅 http/https，打开系统默认浏览器（返回是否可处理）。
pub fn open_external_url(url: String, app: AppHandle) -> Result<bool, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    let parsed = Url::parse(trimmed).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Ok(false);
    }
    app.opener()
        .open_url(trimmed, None::<&str>)
        .map_err(|e| format!("open url failed: {e}"))?;
    Ok(true)
}

// ===========================================================================
// generate_theme：本地 AI 主题生成（Gemini / OpenAI 兼容），镜像 main.cjs
// ===========================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeOptions {
    #[serde(default)]
    is_pure_music: Option<bool>,
    #[serde(default)]
    song_title: Option<String>,
}

const DEFAULT_OPENAI_CHAT_COMPLETIONS_URL: &str = "https://api.openai.com/v1/chat/completions";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";
const DEEPSEEK_DEFAULT_MODEL: &str = "deepseek-v4-flash";
const THEME_JSON_SCHEMA_NAME: &str = "dual_theme";
const DEFAULT_OPENAI_TEMPERATURE: f64 = 0.7;
const GEMINI_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash-preview:generateContent";
const GEMINI_PROVIDER_LABEL: &str = "Google Gemini (Local)";
const OPENAI_PROVIDER_LABEL: &str = "OpenAI Compatible (Local)";
const AI_SNIPPET_CHARS: usize = 2000;

// AI 客户端：env 代理（镜像 Electron 主进程 fetch 走系统代理），无自动重定向，超时更长。
fn build_ai_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn settings_string(store: &SettingsStore, key: &str) -> Option<String> {
    store
        .get(key)
        .and_then(|value| value.as_str().map(|s| s.to_string()))
}

fn settings_temperature(store: &SettingsStore, key: &str) -> Option<f64> {
    match store.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

// 镜像 getGeminiResponseSchema()：Gemini generateContent 的 responseSchema。
fn get_gemini_response_schema() -> Value {
    json!({
        "type": "OBJECT",
        "properties": {
            "light": {
                "type": "OBJECT",
                "description": "Theme optimized for light/daylight mode",
                "properties": {
                    "name": { "type": "STRING", "description": "A creative name for this light theme in Chinese, strictly limited to 10 characters or less" },
                    "description": { "type": "STRING", "description": "A creative 1-sentence description of the mood or visual concept in Chinese, strictly limited to 15 to 30 Chinese characters" },
                    "backgroundColor": { "type": "STRING", "description": "Hex code for light background (whites, creams, pastels)" },
                    "primaryColor": { "type": "STRING", "description": "Hex code for main text (dark color for contrast)" },
                    "accentColor": { "type": "STRING", "description": "Hex code for highlighted text/effects" },
                    "secondaryColor": { "type": "STRING", "description": "Hex code for secondary elements (must contrast with light bg)" },
                    "wordColors": {
                        "type": "ARRAY",
                        "description": "List of exact emotional standalone words from the source text and their specific colors; Latin-script words must not contain punctuation or spaces",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "word": { "type": "STRING" },
                                "color": { "type": "STRING" }
                            },
                            "required": ["word", "color"]
                        }
                    },
                    "lyricsIcons": {
                        "type": "ARRAY",
                        "description": "List of Lucide icon names related to the source text",
                        "items": { "type": "STRING" }
                    }
                },
                "required": ["name", "backgroundColor", "primaryColor", "accentColor", "secondaryColor"]
            },
            "dark": {
                "type": "OBJECT",
                "description": "Theme optimized for dark/midnight mode",
                "properties": {
                    "name": { "type": "STRING", "description": "A creative name for this dark theme in Chinese, strictly limited to 10 characters or less" },
                    "description": { "type": "STRING", "description": "A creative 1-sentence description of the mood or visual concept in Chinese, strictly limited to 15 to 30 Chinese characters" },
                    "backgroundColor": { "type": "STRING", "description": "Hex code for dark background (deep colors)" },
                    "primaryColor": { "type": "STRING", "description": "Hex code for main text (light color for contrast)" },
                    "accentColor": { "type": "STRING", "description": "Hex code for highlighted text/effects" },
                    "secondaryColor": { "type": "STRING", "description": "Hex code for secondary elements (must contrast with dark bg)" },
                    "wordColors": {
                        "type": "ARRAY",
                        "description": "List of exact emotional standalone words from the source text and their specific colors; Latin-script words must not contain punctuation or spaces",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "word": { "type": "STRING" },
                                "color": { "type": "STRING" }
                            },
                            "required": ["word", "color"]
                        }
                    },
                    "lyricsIcons": {
                        "type": "ARRAY",
                        "description": "List of Lucide icon names related to the source text",
                        "items": { "type": "STRING" }
                    }
                },
                "required": ["name", "backgroundColor", "primaryColor", "accentColor", "secondaryColor"]
            }
        },
        "required": ["light", "dark"]
    })
}

// 镜像 THEME_JSON_SCHEMA（OpenAI json_schema / Gemini 兜底共享结构）。
fn get_theme_json_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "light": {
                "type": "object",
                "additionalProperties": false,
                "description": "Theme optimized for light/daylight mode",
                "properties": {
                    "name": { "type": "string", "description": "A creative name for this light theme in Chinese, strictly limited to 10 characters or less" },
                    "description": { "type": "string", "description": "A creative 1-sentence description of the mood or visual concept in Chinese, strictly limited to 15 to 30 Chinese characters" },
                    "backgroundColor": { "type": "string", "description": "Hex code for light background" },
                    "primaryColor": { "type": "string", "description": "Hex code for main text (dark)" },
                    "accentColor": { "type": "string", "description": "Hex code for highlighted text/effects" },
                    "secondaryColor": { "type": "string", "description": "Hex code for secondary elements" },
                    "wordColors": {
                        "type": "array",
                        "description": "List of exact emotional standalone words from the source text and their specific colors; Latin-script words must not contain punctuation or spaces",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "word": { "type": "string" },
                                "color": { "type": "string" }
                            },
                            "required": ["word", "color"]
                        }
                    },
                    "lyricsIcons": {
                        "type": "array",
                        "description": "List of Lucide icon names related to the source text",
                        "items": { "type": "string" }
                    }
                },
                "required": ["name", "backgroundColor", "primaryColor", "accentColor", "secondaryColor", "wordColors", "lyricsIcons"]
            },
            "dark": {
                "type": "object",
                "additionalProperties": false,
                "description": "Theme optimized for dark/midnight mode",
                "properties": {
                    "name": { "type": "string", "description": "A creative name for this dark theme in Chinese, strictly limited to 10 characters or less" },
                    "description": { "type": "string", "description": "A creative 1-sentence description of the mood or visual concept in Chinese, strictly limited to 15 to 30 Chinese characters" },
                    "backgroundColor": { "type": "string", "description": "Hex code for dark background" },
                    "primaryColor": { "type": "string", "description": "Hex code for main text (light)" },
                    "accentColor": { "type": "string", "description": "Hex code for highlighted text/effects" },
                    "secondaryColor": { "type": "string", "description": "Hex code for secondary elements" },
                    "wordColors": {
                        "type": "array",
                        "description": "List of exact emotional standalone words from the source text and their specific colors; Latin-script words must not contain punctuation or spaces",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "word": { "type": "string" },
                                "color": { "type": "string" }
                            },
                            "required": ["word", "color"]
                        }
                    },
                    "lyricsIcons": {
                        "type": "array",
                        "description": "List of Lucide icon names related to the source text",
                        "items": { "type": "string" }
                    }
                },
                "required": ["name", "backgroundColor", "primaryColor", "accentColor", "secondaryColor", "wordColors", "lyricsIcons"]
            }
        },
        "required": ["light", "dark"]
    })
}

fn is_api_version_path(path: &str) -> bool {
    let Some(segment) = path.rsplit('/').next() else {
        return false;
    };
    segment.len() >= 2
        && segment.starts_with('v')
        && segment[1..].bytes().all(|b| b.is_ascii_digit())
}

// 镜像 normalizeOpenAIChatCompletionsUrl。
fn normalize_openai_chat_completions_url(raw_url: &str) -> String {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return DEFAULT_OPENAI_CHAT_COMPLETIONS_URL.to_string();
    }
    match Url::parse(trimmed) {
        Ok(mut parsed) => {
            let normalized_path = parsed.path().trim_end_matches('/').to_string();
            if normalized_path.is_empty() || normalized_path == "/" {
                parsed.set_path("/v1/chat/completions");
            } else if is_api_version_path(&normalized_path) {
                parsed.set_path(&format!("{normalized_path}/chat/completions"));
            } else {
                parsed.set_path(&normalized_path);
            }
            parsed.to_string()
        }
        Err(_) => trimmed.trim_end_matches('/').to_string(),
    }
}

// 镜像 resolveOpenAICompatibleModel。
fn resolve_openai_compatible_model(api_url: &str, configured_model: &str) -> String {
    let trimmed = configured_model.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if let Ok(parsed) = Url::parse(api_url) {
        let hostname = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        if hostname == "api.deepseek.com" || hostname.ends_with(".deepseek.com") {
            return DEEPSEEK_DEFAULT_MODEL.to_string();
        }
    }
    DEFAULT_OPENAI_MODEL.to_string()
}

// 镜像 detectOpenAICompatibleProvider。
fn detect_openai_compatible_provider(api_url: &str, model: &str) -> String {
    let normalized_model = model.trim().to_ascii_lowercase();
    if normalized_model.starts_with("deepseek-") {
        return "deepseek".to_string();
    }
    if let Ok(parsed) = Url::parse(api_url) {
        let hostname = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        if hostname == "api.deepseek.com" || hostname.ends_with(".deepseek.com") {
            return "deepseek".to_string();
        }
        if hostname == "api.openai.com" || hostname.ends_with(".openai.com") {
            return "openai".to_string();
        }
    }
    let prefixes = [
        "gpt", "o1", "o2", "o3", "o4", "o5", "o6", "o7", "o8", "o9", "chatgpt-",
    ];
    if prefixes
        .iter()
        .any(|prefix| normalized_model.starts_with(prefix))
    {
        return "openai".to_string();
    }
    "generic".to_string()
}

fn provider_supports_structured_outputs(provider: &str) -> bool {
    provider == "openai"
}

// 镜像 extractProviderErrorMessage。
fn extract_provider_error_message(payload: &Value) -> Option<String> {
    if !payload.is_object() {
        return None;
    }
    if let Some(error) = payload.get("error") {
        if let Some(text) = error.as_str() {
            return Some(text.to_string());
        }
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            return Some(message.to_string());
        }
    }
    payload
        .get("message")
        .and_then(Value::as_str)
        .map(String::from)
}

// 镜像 formatOpenAICompatibleError。
fn format_openai_compatible_error(status: reqwest::StatusCode, raw_text: &str) -> String {
    let extracted = serde_json::from_str::<Value>(raw_text)
        .ok()
        .and_then(|parsed| extract_provider_error_message(&parsed))
        .filter(|detail| !detail.trim().is_empty());
    let detail = extracted.unwrap_or_else(|| raw_text.trim().to_string());
    if detail.is_empty() {
        format!(
            "OpenAI compatible API error ({}): {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        )
    } else {
        format!(
            "OpenAI compatible API error ({}): {detail}",
            status.as_u16()
        )
    }
}

// 镜像 buildThemeSystemPrompt。
fn build_theme_system_prompt(include_schema_text: bool) -> String {
    let instruction_prompt = "Analyze the mood of the provided song source text and generate TWO visual theme configurations for a music player - one for LIGHT mode and one for DARK mode.

DUAL THEME REQUIREMENTS:
1. Generate TWO complete themes: one optimized for LIGHT/DAYLIGHT mode, one for DARK/MIDNIGHT mode.
2. Both themes should capture the SAME emotional essence of the source text, but with appropriate color palettes for their respective modes.
3. The theme names must be in Chinese and strictly limited to 10 characters or less. They should reflect both the mood AND the mode (e.g., \"忧郁破晓\" for light, \"忧郁子夜\" for dark).
4. The theme description must be a brief, emotional sentence in Chinese (strictly limited to 15 to 30 Chinese characters) reflecting a stream-of-consciousness style with youth and literary characteristics, capturing a listener's immediate emotional reaction to this song. Do not write formal analytical text. Must be written from a first-person listener perspective.
   GUIDELINES FOR THE EXPRESSIVE STYLE:
   - Stream of Consciousness & Literary Vibe: Emphasize poetic, reflective, or introspective thoughts (e.g., emotional connection, existential thoughts, quiet solitude).
   - Youth & Nostalgia: Associate the mood with nostalgic memories of youth, dreams, seasons, or romantic longing.
   - Spatial & Situational Synesthesia: Translate the music's vibe into a vivid situation, atmosphere, weather, or imagery (e.g., summer breeze, starry sky, quiet room).
   Examples for reference: \"戴上耳机的那一刻，喧嚣的世界瞬间消失了。\", \"然后，这份爱编织了太阳和所有星星\", \"你的世界，也包括我在内吗？\", \"微醺的夏夜吹拂过一阵海风。\", \"青春是一种眺望的姿态！\", \"仿佛回到了那个满是汽水味和单车后座的夏天。\".

SOURCE MODE:
1. If 'Pure instrumental' is yes, the source text below is the song title of a pure instrumental track, not lyrics.
2. If 'Pure instrumental' is no, the source text below is a lyrics snippet.
3. Base your mood inference only on the provided source text.

COLOR & THEME GENERATION WORKFLOW:
1. First, identify 10-20 key emotional standalone words from the source text that represent the core mood and atmosphere of the song.
2. Assign a specific, representative color to each of these key emotional standalone words under 'wordColors'.
3. Based on the emotional direction and colors of these identified words, construct the overall color palettes (backgroundColor, primaryColor, secondaryColor, accentColor) for the light and dark themes.
4. Coordinated Colors: The colors assigned in 'wordColors' must be designed in coordination and harmony with the overall color schemes of the themes.

LIGHT THEME RULES:
- Use LIGHT backgrounds. Avoid defaulting to pure white background for every light theme. Generate diverse and rich light-colored backgrounds (e.g., warm creams, soft pastel blues, pale sage greens, gentle peach, warm sands, pale lavenders) that directly match the song's mood.
- Ensure text/icons are dark enough for contrast, but avoid defaulting to pure black (#000000). Generate a very dark tone that coordinates with the background color's hue (e.g., deep navy, dark charcoal, dark plum).
- 'accentColor' must be visible against the light background.

DARK THEME RULES:
- Use DARK backgrounds. Avoid generic pure black backgrounds; use rich, diverse dark colors (e.g., deep midnight blue, dark forest green, charcoal gray, dark plum, deep chocolate, burgundy) matching the song's mood.
- Ensure text/icons are light enough for contrast, but avoid defaulting to pure white (#ffffff). Generate a very bright, soft tone that coordinates with the background color's hue (e.g., soft sky blue, pale mint green, light warm cream).
- 'accentColor' must contrast with the dark background and should be creatively derived from the song's specific mood (e.g., soft blues, mint greens, warm corals, lavender, pale gold) rather than defaulting to generic bright yellow.

SHARED RULES FOR BOTH THEMES:
1. 'secondaryColor': MUST have sufficient contrast against 'backgroundColor'.
2. 'wordColors' and 'lyricsIcons' should be the SAME for both themes (they represent the source text's meaning).

IMPORTANT for 'wordColors':
1. Extract 10-20 emotional standalone words. For Latin-script text, each 'word' MUST be one complete word only, not a phrase.
2. CRITICAL: Do NOT include punctuation, apostrophes, curly quotes, hyphens, or spaces in Latin-script 'word' values. Use clean whole words like \"train\", \"gone\", \"hidden\", \"cities\"; do NOT return \"train's gone\", \"well-hidden\", \"set me free\", or \"shun the light\".
3. Avoid function words such as articles, prepositions, pronouns, particles, and auxiliaries (for example: the, a, an, to, me, and, of, in, on).
4. For CJK lyrics, short meaningful semantic terms may contain multiple CJK characters, but do not select single particles unless they are emotionally meaningful.
5. The 'word' field MUST match text from the source snippet after removing surrounding punctuation. If the pure-instrumental title is very short, using the exact full title as a phrase is allowed.

IMPORTANT for 'lyricsIcons':
1. Identify 3-5 visual concepts/objects mentioned in or strongly implied by the source text.
2. Return them as valid Lucide React icon names (PascalCase).";
    let schema_prompt = if include_schema_text {
        format!(
            "\nResponse MUST be a valid JSON object. Do not include markdown formatting like ```json. Just the raw JSON.\n\nJSON Schema:\n{}",
            serde_json::to_string_pretty(&get_theme_json_schema()).unwrap_or_default()
        )
    } else {
        String::new()
    };
    format!("{instruction_prompt}{schema_prompt}")
}

// 镜像 buildThemeSourcePrompt。
fn build_theme_source_prompt(
    snippet: &str,
    is_pure_music: bool,
    song_title: Option<&str>,
) -> String {
    let mut prompt = format!(
        "Pure instrumental: {}\n",
        if is_pure_music { "yes" } else { "no" }
    );
    if is_pure_music {
        if let Some(title) = song_title {
            prompt.push_str(&format!("Song title: {title}\n"));
        }
    }
    prompt.push_str(&format!("Source snippet:\n{snippet}"));
    prompt
}

// 镜像 resolveOpenAICompatibleTemperature。
fn resolve_openai_compatible_temperature(value: Option<f64>) -> f64 {
    match value {
        Some(temperature) if temperature.is_finite() && (0.0..=2.0).contains(&temperature) => {
            temperature
        }
        _ => DEFAULT_OPENAI_TEMPERATURE,
    }
}

// 镜像 buildOpenAICompatibleRequestBody。
fn build_openai_compatible_request_body(
    model: &str,
    provider: &str,
    system_prompt: &str,
    source_prompt: &str,
    temperature: f64,
) -> Value {
    let messages = json!([
        { "role": "system", "content": system_prompt },
        { "role": "user", "content": source_prompt }
    ]);
    if provider_supports_structured_outputs(provider) {
        json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": THEME_JSON_SCHEMA_NAME,
                    "strict": true,
                    "schema": get_theme_json_schema()
                }
            }
        })
    } else {
        json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "response_format": { "type": "json_object" }
        })
    }
}

// 镜像 extractResponseContentText（含 refusal 拒绝、string/array content）。
fn extract_response_content_text(message: &Value) -> Result<String, String> {
    if !message.is_object() {
        return Err("Failed to generate theme JSON".to_string());
    }
    if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
        if !refusal.trim().is_empty() {
            return Err(format!("Model refused request: {refusal}"));
        }
    }
    if let Some(content) = message.get("content") {
        if let Some(text) = content.as_str() {
            if text.is_empty() {
                return Err("Failed to generate theme JSON".to_string());
            }
            return Ok(text.to_string());
        }
        if let Some(parts) = content.as_array() {
            let text: String = parts
                .iter()
                .filter(|part| part.is_object())
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            if !text.is_empty() {
                return Ok(text);
            }
        }
    }
    Err("Failed to generate theme JSON".to_string())
}

// 镜像 generate-theme 里的 markdown fence 剥离（replace(/^```(json)?\n/,'').replace(/\n```$/,'')）。
fn strip_markdown_fence(raw: &str) -> String {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("```") {
        return raw.to_string();
    }
    let mut result = (&trimmed[3..]).to_string();
    if let Some(rest) = result.strip_prefix("json") {
        result = rest.to_string();
    }
    if let Some(rest) = result.strip_prefix('\n') {
        result = rest.to_string();
    }
    if let Some(prefix) = result.strip_suffix("```") {
        if prefix.ends_with('\n') {
            result = prefix[..prefix.len() - 1].to_string();
        }
    }
    result
}

// 镜像 generateGeminiTheme 的响应提取。
fn extract_gemini_theme_text(data: &Value) -> Result<String, String> {
    let json_text = data
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts
                .iter()
                .find(|part| part.get("text").and_then(Value::as_str).is_some())
        })
        .and_then(|part| part.get("text").and_then(Value::as_str))
        .map(|text| text.to_string());
    json_text.ok_or_else(|| "Failed to generate theme JSON".to_string())
}

// 在 sanitize 之后叠加 provider 标签与默认风格（同 main.cjs generate-theme 尾部）。
fn apply_theme_labels(dual_theme: &mut Value, provider_label: &str) {
    for key in ["light", "dark"] {
        if let Some(theme) = dual_theme.get_mut(key).and_then(Value::as_object_mut) {
            theme.insert(
                "provider".to_string(),
                Value::String(provider_label.to_string()),
            );
            theme.insert("fontStyle".to_string(), Value::String("sans".to_string()));
            theme.insert(
                "animationIntensity".to_string(),
                Value::String("normal".to_string()),
            );
        }
    }
}

#[tauri::command]
// AI 主题生成：按 AI_PROVIDER 走 Gemini 或 OpenAI 兼容接口，返回 sanitize 后的 dual theme。
pub async fn generate_theme(
    lyrics_text: String,
    options: Option<ThemeOptions>,
    state: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let provider = settings_string(&state, "AI_PROVIDER").unwrap_or_else(|| "gemini".to_string());
    let snippet: String = lyrics_text.chars().take(AI_SNIPPET_CHARS).collect();
    let is_pure_music = options
        .as_ref()
        .and_then(|options| options.is_pure_music)
        .unwrap_or(false);
    let song_title = options
        .as_ref()
        .and_then(|options| options.song_title.clone());
    let system_prompt = build_theme_system_prompt(true);
    let source_prompt = build_theme_source_prompt(&snippet, is_pure_music, song_title.as_deref());
    let client = build_ai_http_client();

    let mut dual_theme = if provider == "openai" {
        let api_key = settings_string(&state, "OPENAI_API_KEY")
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| "OPENAI_API_KEY is not configured in settings".to_string())?;
        let api_url = normalize_openai_chat_completions_url(
            &settings_string(&state, "OPENAI_API_URL").unwrap_or_default(),
        );
        let model = resolve_openai_compatible_model(
            &api_url,
            &settings_string(&state, "OPENAI_API_MODEL").unwrap_or_default(),
        );
        let temperature = resolve_openai_compatible_temperature(settings_temperature(
            &state,
            "OPENAI_API_TEMPERATURE",
        ));
        let provider_kind = detect_openai_compatible_provider(&api_url, &model);
        let request_body = build_openai_compatible_request_body(
            &model,
            &provider_kind,
            &system_prompt,
            &source_prompt,
            temperature,
        );

        let response = client
            .post(&api_url)
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {api_key}"))
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("OpenAI compatible API request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let raw_text = response.text().await.unwrap_or_default();
            return Err(format_openai_compatible_error(status, &raw_text));
        }

        let data: Value = response
            .json()
            .await
            .map_err(|e| format!("OpenAI compatible API response parse failed: {e}"))?;
        let content = extract_response_content_text(&data["choices"][0]["message"])?;
        let json_str = strip_markdown_fence(&content);
        let parsed: Value = serde_json::from_str(&json_str)
            .map_err(|e| format!("Failed to parse theme JSON: {e}"))?;
        sanitize_dual_theme(&parsed)
    } else {
        let api_key = settings_string(&state, "GEMINI_API_KEY")
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| "GEMINI_API_KEY is not configured in settings".to_string())?;
        let request_body = json!({
            "systemInstruction": { "parts": [ { "text": system_prompt } ] },
            "contents": [ { "parts": [ { "text": source_prompt } ] } ],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": get_gemini_response_schema()
            }
        });

        let response = client
            .post(GEMINI_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .header("x-goog-api-key", &api_key)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("Gemini API request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let err_text = response.text().await.unwrap_or_default();
            let detail = if err_text.trim().is_empty() {
                String::new()
            } else {
                format!(" - {err_text}")
            };
            return Err(format!(
                "Gemini API error: {} {}{detail}",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            ));
        }

        let data: Value = response
            .json()
            .await
            .map_err(|e| format!("Gemini API response parse failed: {e}"))?;
        let json_text = extract_gemini_theme_text(&data)?;
        let parsed: Value = serde_json::from_str(&json_text)
            .map_err(|e| format!("Failed to parse theme JSON: {e}"))?;
        sanitize_dual_theme(&parsed)
    };

    apply_theme_labels(
        &mut dual_theme,
        if provider == "openai" {
            OPENAI_PROVIDER_LABEL
        } else {
            GEMINI_PROVIDER_LABEL
        },
    );
    Ok(dual_theme)
}

// ---------------------------------------------------------------------------
// themeSanitizer.cjs 的 Rust 移植（用于 generate_theme 返回值清洗）
// ---------------------------------------------------------------------------

fn is_record(value: &Value) -> bool {
    value.is_object()
}

fn is_hex_color(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if !trimmed.starts_with('#') {
        return false;
    }
    let hex = &trimmed[1..];
    if hex.len() != 3 && hex.len() != 6 {
        return false;
    }
    hex.bytes().all(|b| b.is_ascii_hexdigit())
}

fn normalize_hex_color_candidate(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    let trimmed = text.trim();
    if !is_hex_color(trimmed) {
        return None;
    }
    let hex = trimmed[1..].to_ascii_lowercase();
    if hex.len() == 3 {
        let mut out = String::with_capacity(7);
        out.push('#');
        for byte in hex.bytes() {
            let character = byte as char;
            out.push(character);
            out.push(character);
        }
        return Some(out);
    }
    Some(format!("#{hex}"))
}

fn normalize_theme_hex_color(value: Option<&Value>, fallback: &str, hard_fallback: &str) -> String {
    normalize_hex_color_candidate(value)
        .or_else(|| normalize_hex_color_candidate(Some(&Value::String(fallback.to_string()))))
        .unwrap_or_else(|| hard_fallback.to_string())
}

fn normalize_font_style(value: Option<&Value>, fallback: &str) -> String {
    let normalized = value
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "serif" | "mono" | "sans"));
    match normalized {
        Some(value) => value.to_string(),
        None => fallback.to_string(),
    }
}

fn normalize_animation_intensity(value: Option<&Value>, fallback: &str) -> String {
    let normalized = value
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "calm" | "chaotic" | "normal"));
    match normalized {
        Some(value) => value.to_string(),
        None => fallback.to_string(),
    }
}

fn normalize_word_colors(value: Option<&Value>, fallback_color: &str) -> Value {
    let Some(array) = value.and_then(Value::as_array) else {
        return json!([]);
    };
    let mut out = Vec::new();
    for entry in array {
        if !entry.is_object() {
            continue;
        }
        let word = entry
            .get("word")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if word.is_empty() {
            continue;
        }
        let color = normalize_theme_hex_color(entry.get("color"), fallback_color, "#ffffff");
        out.push(json!({ "word": word, "color": color }));
    }
    Value::Array(out)
}

fn normalize_lyrics_icons(value: Option<&Value>) -> Value {
    let Some(array) = value.and_then(Value::as_array) else {
        return json!([]);
    };
    let icons: Vec<Value> = array
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|icon| !icon.is_empty())
        .take(12)
        .map(|icon| Value::String(icon.to_string()))
        .collect();
    Value::Array(icons)
}

fn theme_field<'a>(source: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    source.and_then(|source| source.get(key))
}

fn sanitize_theme(source: Option<&Value>, fallback: &ThemeFallback) -> Value {
    let accent_color = normalize_theme_hex_color(
        theme_field(source, "accentColor"),
        &fallback.accent_color,
        "#ffffff",
    );
    let name = theme_field(source, "name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&fallback.name);
    let provider = theme_field(source, "provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .unwrap_or(&fallback.provider);

    let mut object = Map::new();
    object.insert("name".to_string(), Value::String(name.to_string()));
    if let Some(description) = theme_field(source, "description").and_then(Value::as_str) {
        object.insert(
            "description".to_string(),
            Value::String(description.trim().to_string()),
        );
    }
    object.insert(
        "backgroundColor".to_string(),
        Value::String(normalize_theme_hex_color(
            theme_field(source, "backgroundColor"),
            &fallback.background_color,
            "#ffffff",
        )),
    );
    object.insert(
        "primaryColor".to_string(),
        Value::String(normalize_theme_hex_color(
            theme_field(source, "primaryColor"),
            &fallback.primary_color,
            "#ffffff",
        )),
    );
    object.insert(
        "accentColor".to_string(),
        Value::String(accent_color.clone()),
    );
    object.insert(
        "secondaryColor".to_string(),
        Value::String(normalize_theme_hex_color(
            theme_field(source, "secondaryColor"),
            &fallback.secondary_color,
            "#ffffff",
        )),
    );
    object.insert(
        "fontStyle".to_string(),
        Value::String(normalize_font_style(
            theme_field(source, "fontStyle"),
            &fallback.font_style,
        )),
    );
    object.insert(
        "animationIntensity".to_string(),
        Value::String(normalize_animation_intensity(
            theme_field(source, "animationIntensity"),
            &fallback.animation_intensity,
        )),
    );
    object.insert(
        "wordColors".to_string(),
        normalize_word_colors(theme_field(source, "wordColors"), &accent_color),
    );
    object.insert(
        "lyricsIcons".to_string(),
        normalize_lyrics_icons(theme_field(source, "lyricsIcons")),
    );
    object.insert("provider".to_string(), Value::String(provider.to_string()));
    Value::Object(object)
}

struct ThemeFallback {
    name: String,
    background_color: String,
    primary_color: String,
    accent_color: String,
    secondary_color: String,
    font_style: String,
    animation_intensity: String,
    provider: String,
}

fn light_theme_fallback() -> ThemeFallback {
    ThemeFallback {
        name: "AI Light".to_string(),
        background_color: "#ffffff".to_string(),
        primary_color: "#111827".to_string(),
        accent_color: "#2563eb".to_string(),
        secondary_color: "#475569".to_string(),
        font_style: "sans".to_string(),
        animation_intensity: "normal".to_string(),
        provider: "AI".to_string(),
    }
}

fn dark_theme_fallback() -> ThemeFallback {
    ThemeFallback {
        name: "AI Dark".to_string(),
        background_color: "#0f172a".to_string(),
        primary_color: "#f8fafc".to_string(),
        accent_color: "#7dd3fc".to_string(),
        secondary_color: "#cbd5e1".to_string(),
        font_style: "sans".to_string(),
        animation_intensity: "normal".to_string(),
        provider: "AI".to_string(),
    }
}

// 镜像 sanitizeDualTheme：非对象输入回退默认 dual theme。
fn sanitize_dual_theme(value: &Value) -> Value {
    let source = if is_record(value) {
        value
    } else {
        &Value::Null
    };
    json!({
        "light": sanitize_theme(theme_field(Some(source), "light"), &light_theme_fallback()),
        "dark": sanitize_theme(theme_field(Some(source), "dark"), &dark_theme_fallback())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::{routing::get, routing::post, Json, Router};
    use base64::Engine;
    use std::net::SocketAddr;

    fn header_map(pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert(key.to_string(), Value::String(value.to_string()));
        }
        map
    }

    // ------------------------------------------------------------------
    // 白名单 / 校验（纯函数）
    // ------------------------------------------------------------------

    #[test]
    fn allowlist_exact_and_subdomains() {
        for allowed in [
            "qq.com",
            "c.y.qq.com",
            "y.qq.com",
            "y.gtimg.cn",
            "kugou.com",
            "complexsearch.kugou.com",
            "fs.kugou.com",
            "kgimg.com",
            "imge.kugou.com.kgimg.com",
            "amll-ttml-db.stevexmh.net",
            "QQ.COM",
        ] {
            assert!(
                is_allowed_lyric_proxy_host(allowed),
                "{allowed} should be allowed"
            );
        }
        for denied in [
            "evil.com",
            "qq.com.evil.com",
            "evilqq.com",
            "notkugou.com",
            "kugou.com.evil.com",
            "amll-ttml-db.stevexmh.net.evil.com",
            "stevexmh.net",
            "y.gtimg.cn.evil.com",
            "localhost",
            "127.0.0.1",
        ] {
            assert!(
                !is_allowed_lyric_proxy_host(denied),
                "{denied} should be denied"
            );
        }
    }

    #[test]
    fn amll_host_detection_is_exact() {
        assert!(is_amll_db_host("amll-ttml-db.stevexmh.net"));
        assert!(is_amll_db_host("AMLL-TTML-DB.STEVEXMH.NET"));
        assert!(!is_amll_db_host("amll-ttml-db.stevexmh.net.evil.com"));
        assert!(!is_amll_db_host("www.kugou.com"));
    }

    #[test]
    fn method_validation() {
        for method in [
            "GET", "get", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS",
        ] {
            assert!(is_http_method(method), "{method} should be allowed");
        }
        for method in ["CONNECT", "TRACE", "FOO", "GET / HTTP/1.1"] {
            assert!(!is_http_method(method), "{method} should be rejected");
        }
    }

    #[test]
    fn header_sanitizer_drops_forbidden_and_hop_by_hop() {
        let headers = header_map(&[
            ("Host", "fake.example"),
            ("Connection", "keep-alive"),
            ("Content-Length", "999"),
            ("origin", "https://tauri.local"),
            ("Referer", "https://tauri.local/"),
            ("Transfer-Encoding", "chunked"),
            ("X-Custom", "yes"),
            ("User-Agent", "test"),
        ]);
        let sanitized = sanitize_forward_headers(&headers).unwrap();
        let names: Vec<&str> = sanitized.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"X-Custom"));
        assert!(names.contains(&"User-Agent"));
    }

    #[test]
    fn header_sanitizer_rejects_invalid_names_and_values() {
        assert!(sanitize_forward_headers(&header_map(&[("Bad Name", "x")])).is_err());
        assert!(sanitize_forward_headers(&header_map(&[("x", "line1\r\nX: 1")])).is_err());
        assert!(sanitize_forward_headers(&header_map(&[("x", "line1\nX: 1")])).is_err());

        let non_string = header_map(&[("x", "ok")]);
        let mut bad = non_string;
        bad.insert("y".to_string(), json!(["a", "b"]));
        assert!(sanitize_forward_headers(&bad).is_err());
    }

    #[test]
    fn header_sanitizer_enforces_size_and_count_limits() {
        let mut many = Map::new();
        for i in 0..100 {
            many.insert(format!("x-{i}"), Value::String("v".to_string()));
        }
        assert!(sanitize_forward_headers(&many).is_err());

        let huge = header_map(&[("x-big", &"a".repeat(MAX_HEADER_BYTES + 10))]);
        assert!(sanitize_forward_headers(&huge).is_err());
    }

    #[test]
    fn redirect_resolution_handles_relative_and_scheme() {
        let current = Url::parse("http://amll-ttml-db.stevexmh.net/a/b?q=1").unwrap();
        let absolute = resolve_redirect_url(&current, "http://complexsearch.kugou.com/x").unwrap();
        assert_eq!(absolute.host_str(), Some("complexsearch.kugou.com"));
        // RFC 3986 相对解析：base 视为文件，../c 从 /a/b 上溯后落到根路径。
        let relative = resolve_redirect_url(&current, "../c").unwrap();
        assert_eq!(relative.path(), "/c");
        assert!(matches!(
            resolve_redirect_url(&current, "file:///etc/passwd"),
            Err(LyricProxyError::Forbidden(_))
        ));
        assert!(matches!(
            resolve_redirect_url(&current, "ftp://kugou.com/x"),
            Err(LyricProxyError::Forbidden(_))
        ));
        assert!(matches!(
            resolve_redirect_url(&current, "http://"),
            Err(LyricProxyError::BadRequest(_))
        ));
    }

    // ------------------------------------------------------------------
    // AI 主题生成辅助（纯函数）
    // ------------------------------------------------------------------

    #[test]
    fn openai_url_normalization() {
        assert_eq!(
            normalize_openai_chat_completions_url(""),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_chat_completions_url("https://myhost.example"),
            "https://myhost.example/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_chat_completions_url("https://myhost.example/v1/"),
            "https://myhost.example/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_chat_completions_url("https://myhost.example/other"),
            "https://myhost.example/other"
        );
        assert_eq!(
            normalize_openai_chat_completions_url("https://myhost.example/v1/chat/completions"),
            "https://myhost.example/v1/chat/completions"
        );
    }

    #[test]
    fn openai_model_resolution() {
        assert_eq!(
            resolve_openai_compatible_model("https://api.deepseek.com", ""),
            "deepseek-v4-flash"
        );
        assert_eq!(
            resolve_openai_compatible_model("https://api.openai.com/v1/chat/completions", ""),
            "gpt-4o"
        );
        assert_eq!(
            resolve_openai_compatible_model(
                "https://api.openai.com/v1/chat/completions",
                "  my-model  "
            ),
            "my-model"
        );
    }

    #[test]
    fn openai_provider_detection() {
        assert_eq!(
            detect_openai_compatible_provider(
                "https://api.openai.com/v1/chat/completions",
                "gpt-4o"
            ),
            "openai"
        );
        assert_eq!(
            detect_openai_compatible_provider("https://api.deepseek.com", "deepseek-chat"),
            "deepseek"
        );
        assert_eq!(
            detect_openai_compatible_provider("https://myhost.example/v1", "some-model"),
            "generic"
        );
        assert_eq!(
            detect_openai_compatible_provider("https://myhost.example/v1", "o3-mini"),
            "openai"
        );
    }

    #[test]
    fn openai_temperature_resolution() {
        assert_eq!(resolve_openai_compatible_temperature(Some(0.5)), 0.5);
        assert_eq!(resolve_openai_compatible_temperature(Some(0.0)), 0.0);
        assert_eq!(resolve_openai_compatible_temperature(Some(2.0)), 2.0);
        assert_eq!(
            resolve_openai_compatible_temperature(Some(-1.0)),
            DEFAULT_OPENAI_TEMPERATURE
        );
        assert_eq!(
            resolve_openai_compatible_temperature(Some(3.0)),
            DEFAULT_OPENAI_TEMPERATURE
        );
        assert_eq!(
            resolve_openai_compatible_temperature(None),
            DEFAULT_OPENAI_TEMPERATURE
        );
        assert_eq!(
            resolve_openai_compatible_temperature(Some(f64::NAN)),
            DEFAULT_OPENAI_TEMPERATURE
        );
    }

    #[test]
    fn markdown_fence_stripping() {
        assert_eq!(strip_markdown_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_markdown_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_markdown_fence("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(
            strip_markdown_fence("```json\n{\"a\":1}\n```extra"),
            "{\"a\":1}\n```extra"
        );
    }

    #[test]
    fn response_content_extraction() {
        let message = json!({ "content": "plain text" });
        assert_eq!(
            extract_response_content_text(&message).unwrap(),
            "plain text"
        );

        let message = json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "image_url", "image_url": { "url": "x" } },
                { "type": "text", "text": " world" }
            ]
        });
        assert_eq!(
            extract_response_content_text(&message).unwrap(),
            "hello world"
        );

        let message = json!({ "content": "" });
        assert!(extract_response_content_text(&message).is_err());

        let message = json!({ "refusal": "I refuse" });
        assert!(extract_response_content_text(&message).is_err());

        assert!(extract_response_content_text(&json!({})).is_err());
    }

    #[test]
    fn theme_source_prompt_building() {
        let lyrics = build_theme_source_prompt("hello world", false, None);
        assert_eq!(
            lyrics,
            "Pure instrumental: no\nSource snippet:\nhello world"
        );
        let instrumental = build_theme_source_prompt("rain", true, Some("Rain Song"));
        assert_eq!(
            instrumental,
            "Pure instrumental: yes\nSong title: Rain Song\nSource snippet:\nrain"
        );
        let instrumental_no_title = build_theme_source_prompt("rain", true, None);
        assert_eq!(
            instrumental_no_title,
            "Pure instrumental: yes\nSource snippet:\nrain"
        );
    }

    #[test]
    fn dual_theme_sanitizer_normalizes_shapes() {
        let raw = json!({
            "light": {
                "name": " 破晓 ",
                "backgroundColor": "#fff",
                "primaryColor": "#000000",
                "accentColor": "not-a-color",
                "secondaryColor": "#123456",
                "wordColors": [
                    { "word": "star", "color": "#FFF" },
                    { "word": "", "color": "#000000" },
                    { "word": "rain", "color": "garbage" }
                ],
                "lyricsIcons": ["  Music2 ", "heart", "", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"],
                "fontStyle": "serif",
                "animationIntensity": "chaotic"
            },
            "dark": { "backgroundColor": "#0f172a", "name": 42 }
        });
        let sanitized = sanitize_dual_theme(&raw);
        let light = &sanitized["light"];
        assert_eq!(light["name"], "破晓");
        assert_eq!(light["backgroundColor"], "#ffffff");
        assert_eq!(light["primaryColor"], "#000000");
        assert_eq!(light["accentColor"], "#2563eb");
        assert_eq!(light["fontStyle"], "serif");
        assert_eq!(light["animationIntensity"], "chaotic");
        assert_eq!(light["wordColors"][0]["color"], "#ffffff");
        assert_eq!(light["wordColors"].as_array().unwrap().len(), 2);
        assert_eq!(light["lyricsIcons"].as_array().unwrap().len(), 12);
        assert_eq!(light["provider"], "AI");

        let dark = &sanitized["dark"];
        assert_eq!(dark["name"], "AI Dark");
        assert_eq!(dark["backgroundColor"], "#0f172a");
    }

    #[test]
    fn dual_theme_sanitizer_falls_back_for_non_object() {
        let sanitized = sanitize_dual_theme(&json!([1, 2, 3]));
        assert_eq!(sanitized["light"]["name"], "AI Light");
        assert_eq!(sanitized["dark"]["name"], "AI Dark");
    }

    #[test]
    fn theme_labels_are_applied_after_sanitize() {
        let mut theme = sanitize_dual_theme(&json!({}));
        apply_theme_labels(&mut theme, OPENAI_PROVIDER_LABEL);
        assert_eq!(theme["light"]["provider"], "OpenAI Compatible (Local)");
        assert_eq!(theme["light"]["fontStyle"], "sans");
        assert_eq!(theme["light"]["animationIntensity"], "normal");
        assert_eq!(theme["dark"]["provider"], "OpenAI Compatible (Local)");
    }

    #[test]
    fn openai_error_formatting_extracts_provider_message() {
        let err = format_openai_compatible_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"bad api key"}}"#,
        );
        assert!(
            err.contains("OpenAI compatible API error (400): bad api key"),
            "{err}"
        );
        let err = format_openai_compatible_error(reqwest::StatusCode::BAD_REQUEST, "");
        assert!(err.contains("OpenAI compatible API error (400)"), "{err}");
    }

    // ------------------------------------------------------------------
    // 端到端（本地 HTTP 服务器 + DNS 覆盖，真实请求路径）
    // ------------------------------------------------------------------

    struct TestServer {
        port: u16,
        handle: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let app = test_router(port);
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self { port, handle }
        }

        fn url(&self, host: &str, path: &str) -> String {
            format!(
                "http://{host}:{}/{}",
                self.port,
                path.trim_start_matches('/')
            )
        }

        fn client(&self) -> reqwest::Client {
            let addr: SocketAddr = format!("127.0.0.1:{}", self.port).parse().unwrap();
            base_client_builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .resolve_to_addrs("amll-ttml-db.stevexmh.net", &[addr])
                .resolve_to_addrs("complexsearch.kugou.com", &[addr])
                .resolve_to_addrs("www.kugou.com", &[addr])
                .build()
                .unwrap()
        }
    }

    fn test_router(port: u16) -> Router {
        Router::new()
            .route("/", get(|| async { "root" }))
            .route(
                "/amll-404",
                get(|| async { (StatusCode::NOT_FOUND, "not found") }),
            )
            .route("/final", get(|| async { ("final-ok",) }))
            .route(
                "/binary",
                get(|| async {
                    (
                        StatusCode::OK,
                        [("content-type", "application/octet-stream")],
                        vec![0xffu8, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46],
                    )
                }),
            )
            .route(
                "/redirect-kugou",
                get(move || async move {
                    (
                        StatusCode::FOUND,
                        [(
                            "Location",
                            format!("http://complexsearch.kugou.com:{port}/final"),
                        )],
                        "",
                    )
                }),
            )
            .route(
                "/redirect-evil",
                get(move || async move {
                    (
                        StatusCode::FOUND,
                        [("Location", format!("http://evil.com:{port}/final"))],
                        "",
                    )
                }),
            )
            .route(
                "/redirect-file",
                get(|| async { (StatusCode::FOUND, [("Location", "file:///etc/passwd")], "") }),
            )
            .route(
                "/redirect-relative",
                get(|| async { (StatusCode::FOUND, [("Location", "/final")], "") }),
            )
            .route(
                "/redirect-loop",
                get(|| async { (StatusCode::FOUND, [("Location", "/redirect-loop")], "") }),
            )
            .route(
                "/echo",
                get(|headers: HeaderMap| async move {
                    let mut map = Map::new();
                    for (name, value) in headers.iter() {
                        map.insert(
                            name.as_str().to_ascii_lowercase(),
                            Value::String(value.to_str().unwrap_or_default().to_string()),
                        );
                    }
                    Json(map)
                }),
            )
            .route(
                "/hopbyhop",
                get(|| async {
                    (
                        StatusCode::OK,
                        [
                            ("x-foo", "bar"),
                            ("keep-alive", "timeout=5"),
                            ("connection", "keep-alive"),
                            ("trailer", "x-foo"),
                        ],
                        "hop",
                    )
                }),
            )
            .route(
                "/post",
                post(|headers: HeaderMap, body: String| async move {
                    let content_type = headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    (
                        [("x-echo-content-type", content_type)],
                        format!("echo:{body}"),
                    )
                }),
            )
    }

    async fn fetch(
        client: &reqwest::Client,
        url: String,
        init: Option<LyricProxyInit>,
    ) -> Result<LyricProxyResponse, String> {
        into_command_result(lyric_proxy_fetch_inner(client, url, init).await)
    }

    #[tokio::test]
    async fn allowed_host_is_proxied() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(&client, server.url("www.kugou.com", "/"), None)
            .await
            .unwrap();
        assert_eq!(result.status, 200);
        assert!(result.ok);
        assert_eq!(result.body_text, "root");
        server.handle.abort();
    }

    #[tokio::test]
    async fn non_whitelisted_host_is_forbidden() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(&client, server.url("evil.com", "/"), None)
            .await
            .unwrap();
        assert_eq!(result.status, 403);
        assert!(!result.ok);
        assert!(
            result.body_text.contains("Forbidden lyric proxy host"),
            "{}",
            result.body_text
        );
        server.handle.abort();
    }

    #[tokio::test]
    async fn bad_scheme_and_unparseable_url_reject() {
        let server = TestServer::start().await;
        let client = server.client();
        assert!(fetch(&client, "ftp://kugou.com/x".to_string(), None)
            .await
            .is_err());
        assert!(fetch(&client, "not a url".to_string(), None).await.is_err());
        assert!(fetch(&client, "http://".to_string(), None).await.is_err());
        server.handle.abort();
    }

    #[tokio::test]
    async fn amll_404_becomes_204() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(
            &client,
            server.url("amll-ttml-db.stevexmh.net", "/amll-404"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 204);
        assert!(result.ok);
        assert_eq!(result.body_text, "");
        server.handle.abort();
    }

    #[tokio::test]
    async fn non_amll_404_stays_404() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(&client, server.url("www.kugou.com", "/amll-404"), None)
            .await
            .unwrap();
        assert_eq!(result.status, 404);
        assert!(!result.ok);
        server.handle.abort();
    }

    #[tokio::test]
    async fn redirect_within_allowlist_is_followed() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(
            &client,
            server.url("amll-ttml-db.stevexmh.net", "/redirect-kugou"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.body_text, "final-ok");
        server.handle.abort();
    }

    #[tokio::test]
    async fn relative_redirect_is_followed_within_allowlist() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(
            &client,
            server.url("www.kugou.com", "/redirect-relative"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.body_text, "final-ok");
        server.handle.abort();
    }

    #[tokio::test]
    async fn redirect_escaping_allowlist_is_blocked() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(
            &client,
            server.url("amll-ttml-db.stevexmh.net", "/redirect-evil"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 403);
        assert!(!result.ok);
        server.handle.abort();
    }

    #[tokio::test]
    async fn redirect_to_non_http_scheme_is_blocked() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(&client, server.url("www.kugou.com", "/redirect-file"), None)
            .await
            .unwrap();
        assert_eq!(result.status, 403);
        server.handle.abort();
    }

    #[tokio::test]
    async fn redirect_loop_hits_limit_and_is_blocked() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(&client, server.url("www.kugou.com", "/redirect-loop"), None)
            .await
            .unwrap();
        assert_eq!(result.status, 403);
        assert!(result.body_text.contains("Too many redirects"));
        server.handle.abort();
    }

    #[tokio::test]
    async fn request_headers_are_sanitized_on_the_wire() {
        let server = TestServer::start().await;
        let client = server.client();
        let init = LyricProxyInit {
            method: Some("GET".to_string()),
            headers: Some(header_map(&[
                ("Host", "fake.example"),
                ("Connection", "keep-alive"),
                ("Content-Length", "999"),
                ("Origin", "https://tauri.local"),
                ("Referer", "https://tauri.local/"),
                ("X-Custom", "yes"),
                ("User-Agent", "test-agent"),
            ])),
            body: None,
        };
        let result = fetch(&client, server.url("www.kugou.com", "/echo"), Some(init))
            .await
            .unwrap();
        let echoed: Value = serde_json::from_str(&result.body_text).unwrap();
        assert_eq!(echoed["x-custom"], "yes");
        assert_eq!(echoed["user-agent"], "test-agent");
        // reqwest 会用目标 URL 重建 Host（白名单域本身），调用方伪造的 Host 不得转发。
        let expected_host = format!("www.kugou.com:{}", server.port);
        assert_eq!(echoed["host"], expected_host);
        for stripped in ["connection", "content-length", "origin", "referer"] {
            assert!(
                echoed.get(stripped).is_none(),
                "header {stripped} should not be forwarded: {echoed}"
            );
        }
        server.handle.abort();
    }

    #[tokio::test]
    async fn post_body_and_content_type_are_forwarded() {
        let server = TestServer::start().await;
        let client = server.client();
        let init = LyricProxyInit {
            method: Some("POST".to_string()),
            headers: Some(header_map(&[("Content-Type", "application/json")])),
            body: Some("{\"q\":1}".to_string()),
        };
        let result = fetch(&client, server.url("www.kugou.com", "/post"), Some(init))
            .await
            .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.body_text, "echo:{\"q\":1}");
        let content_type = result.headers.get("x-echo-content-type").unwrap();
        assert_eq!(content_type, "application/json");
        server.handle.abort();
    }

    #[tokio::test]
    async fn binary_body_is_preserved_in_body_data_base64() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(&client, server.url("www.kugou.com", "/binary"), None)
            .await
            .unwrap();
        assert_eq!(result.status, 200);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result.body_data.as_ref().unwrap())
            .unwrap();
        assert_eq!(
            decoded,
            vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46]
        );
        // bodyText 是非 UTF-8 时的 lossy 视图（与 base64 原始字节不同），仅供文本调用方使用。
        assert_eq!(result.body_text, String::from_utf8_lossy(&decoded));
        assert!(
            decoded.iter().any(|byte| *byte > 0x7f),
            "binary body must contain non-ASCII bytes to exercise the lossy path"
        );
        server.handle.abort();
    }

    #[tokio::test]
    async fn response_headers_drop_hop_by_hop() {
        let server = TestServer::start().await;
        let client = server.client();
        let result = fetch(&client, server.url("www.kugou.com", "/hopbyhop"), None)
            .await
            .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.headers.get("x-foo").unwrap(), "bar");
        for stripped in ["connection", "keep-alive", "trailer", "transfer-encoding"] {
            assert!(
                !result.headers.contains_key(stripped),
                "hop-by-hop header {stripped} should be stripped: {:?}",
                result.headers
            );
        }
        server.handle.abort();
    }

    #[tokio::test]
    async fn unsupported_method_rejects() {
        let server = TestServer::start().await;
        let client = server.client();
        let init = LyricProxyInit {
            method: Some("TRACE".to_string()),
            headers: None,
            body: None,
        };
        assert!(fetch(&client, server.url("www.kugou.com", "/"), Some(init))
            .await
            .is_err());
        server.handle.abort();
    }

    #[tokio::test]
    async fn oversized_body_rejects() {
        let server = TestServer::start().await;
        let client = server.client();
        let init = LyricProxyInit {
            method: Some("POST".to_string()),
            headers: None,
            body: Some("a".repeat(MAX_BODY_BYTES + 1)),
        };
        assert!(
            fetch(&client, server.url("www.kugou.com", "/post"), Some(init))
                .await
                .is_err()
        );
        server.handle.abort();
    }

    #[tokio::test]
    #[ignore = "live read-only smoke via proxy; run with cargo test -- --ignored"]
    async fn live_smoke() {
        let client = build_lyric_proxy_client();
        for url in [
            "https://www.kugou.com/",
            "https://y.qq.com/",
            "https://amll-ttml-db.stevexmh.net/ncm/33894312?format=ttml",
            // M12 联调目标：酷狗匿名搜索（WebView2 CORS 中继的典型用例）。
            "http://complexsearch.kugou.com/v2/search/song?keyword=hello&page=1&pagesize=2&userid=0&appid=3116&token=&clienttime=0&iscorrection=1&uuid=-&dfid=-&clientver=11070&platform=AndroidFilter",
        ] {
            match fetch(&client, url.to_string(), None).await {
                Ok(response) => println!(
                    "SMOKE {url} -> status={} ok={} bytes={}",
                    response.status,
                    response.ok,
                    response.body_text.len()
                ),
                Err(error) => println!("SMOKE {url} -> ERROR {error}"),
            }
        }
        let forbidden = fetch(&client, "https://example.com/".to_string(), None)
            .await
            .unwrap();
        assert_eq!(forbidden.status, 403);
        println!("SMOKE https://example.com/ -> status=403 (blocked)");

        let kugou = fetch(&client, "https://www.kugou.com/".to_string(), None)
            .await
            .unwrap();
        assert_eq!(kugou.status, 200);
        println!("SMOKE https://www.kugou.com/ -> 200 OK");
    }
}
