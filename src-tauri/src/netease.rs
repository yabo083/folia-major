//! M3 Netease API: loopback HTTP server that proxies folia-tauri's frontend
//! REST calls to music.163.com using the weapi transport.
//!
//! Behavior follows `specs/M3-netease-api.md` and the reference package
//! `@neteasecloudmusicapienhanced/api` (see `netease/routes.rs` and
//! `netease/crypto.rs`). The server binds `127.0.0.1:0`, announces the bound
//! port through the `netease-api-status-changed` event, and answers the
//! `get_netease_port` / `get_netease_api_status` commands.

pub mod crypto;
pub mod routes;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use base64::Engine as _;
use rand::{Rng, RngCore};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::netease::crypto::{eapi_encrypt, eapi_res_decrypt, random_secret_key, weapi_encrypt};
use crate::netease::routes::{find_route, Protocol, Route, RouteKind};

const DOMAIN: &str = "https://music.163.com";
const API_DOMAIN: &str = "https://interface.music.163.com";
const WEAPI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0";
const CHECK_TOKEN: &str = "9ca17ae2e6ffcda170e2e6ee8af14fbabdb988f225b3868eb2c15a879b9a83d274a790ac8ff54a97b889d5d42af0feaec3b92af58cff99c470a7eafd88f75e839a9ea7c14e909da883e83fb692a3abdb6b92adee9e";

const OS_OSVER: &str = "Microsoft-Windows-10-Professional-build-19045-64bit";
const OS_APPVER: &str = "3.1.17.204416";
const OS_CHANNEL: &str = "netease";
const OS_OS: &str = "pc";

const MAX_URI_LEN: usize = 8192;
const MAX_QUERY_LEN: usize = 4096;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);
const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);
const ALLOWED_WEBVIEW_ORIGINS: &[&str] = &[
    "http://tauri.localhost",
    "https://tauri.localhost",
    "tauri://localhost",
    "http://localhost:3000",
    "http://127.0.0.1:3000",
];

const STATUS_EVENT: &str = "netease-api-status-changed";

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub status: String,
    pub port: Option<u16>,
    pub error: Option<String>,
    pub updated_at: i64,
}

struct NeteaseApiInner {
    status: Mutex<ServerStatus>,
    port: AtomicU16,
    cookie: RwLock<Option<String>>,
    client: reqwest::Client,
}

/// Managed by Tauri (`app.manage`); shared with the `get_netease_port` and
/// `get_netease_api_status` commands and the HTTP server task.
#[derive(Clone)]
pub struct NeteaseApiState {
    inner: Arc<NeteaseApiInner>,
}

impl NeteaseApiState {
    pub fn new() -> Self {
        let proxy = proxy_from_env().unwrap_or_else(|| "http://127.0.0.1:7890".to_string());
        let mut builder = reqwest::Client::builder()
            .user_agent(WEAPI_UA)
            .timeout(UPSTREAM_TIMEOUT)
            .connect_timeout(Duration::from_secs(10));
        if let Ok(proxy) = reqwest::Proxy::all(&proxy) {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().unwrap_or_else(|_| reqwest::Client::new());
        Self {
            inner: Arc::new(NeteaseApiInner {
                status: Mutex::new(ServerStatus {
                    status: "starting".to_string(),
                    port: None,
                    error: None,
                    updated_at: 0,
                }),
                port: AtomicU16::new(0),
                cookie: RwLock::new(None),
                client,
            }),
        }
    }

    pub fn port(&self) -> u16 {
        self.inner.port.load(Ordering::SeqCst)
    }

    pub fn status_json(&self) -> Value {
        let status = self.inner.status.lock().unwrap();
        serde_json::to_value(&*status).unwrap_or_else(|_| json!({}))
    }

    fn set_status(&self, status: ServerStatus) {
        if let Some(port) = status.port {
            self.inner.port.store(port, Ordering::SeqCst);
        }
        *self.inner.status.lock().unwrap() = status;
    }

    fn emit_status(&self, app: &AppHandle) {
        let _ = app.emit(STATUS_EVENT, self.status_json());
    }

    fn persist_cookie(&self, cookie: String) {
        if let Ok(mut guard) = self.inner.cookie.write() {
            *guard = Some(cookie);
        }
    }

    fn persisted_cookie(&self) -> Option<String> {
        self.inner.cookie.read().ok().and_then(|g| g.clone())
    }

    fn merge_upstream_cookies(&self, set_cookies: &[String]) {
        if set_cookies.is_empty() {
            return;
        }
        let current = self.persisted_cookie();
        let merged = merge_cookies(current.as_deref(), set_cookies);
        self.persist_cookie(merged);
    }
}

fn proxy_from_env() -> Option<String> {
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
    ] {
        if let Ok(val) = std::env::var(key) {
            let val = val.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// 返回 Netease 本地回环 API 服务器的真实端口（尚未启动时为 0）。
#[tauri::command]
pub fn get_netease_port(state: tauri::State<'_, NeteaseApiState>) -> u16 {
    state.port()
}

/// 返回 Netease 本地回环 API 服务器当前状态。
#[tauri::command]
pub fn get_netease_api_status(state: tauri::State<'_, NeteaseApiState>) -> Value {
    state.status_json()
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

/// Owns the tokio runtime that drives the axum server for the app's lifetime.
pub struct NeteaseApiServer {
    _runtime: Option<tokio::runtime::Runtime>,
}

impl NeteaseApiServer {
    /// Bind `127.0.0.1:0`, record the real port, emit the status event, and
    /// start serving in a background tokio runtime. On failure the status is
    /// set to `error` and the event is still emitted (the frontend toasts).
    pub fn start(app: &AppHandle, state: NeteaseApiState) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("failed to create tokio runtime: {e}"))?;

        let router = build_router(state.clone());
        let bind_result = runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
            let port = listener.local_addr()?.port();
            tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, router).await {
                    eprintln!("[netease] server stopped: {e}");
                }
            });
            Ok::<u16, std::io::Error>(port)
        });

        let now = now_ms();
        match bind_result {
            Ok(port) => {
                state.set_status(ServerStatus {
                    status: "running".to_string(),
                    port: Some(port),
                    error: None,
                    updated_at: now,
                });
                state.emit_status(app);
                eprintln!("[netease] API server running on http://127.0.0.1:{port}");
                Ok(Self {
                    _runtime: Some(runtime),
                })
            }
            Err(e) => {
                state.set_status(ServerStatus {
                    status: "error".to_string(),
                    port: None,
                    error: Some(e.to_string()),
                    updated_at: now,
                });
                state.emit_status(app);
                Err(format!("netease API server failed to start: {e}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

fn build_router(state: NeteaseApiState) -> Router {
    Router::new()
        .fallback(handle_request)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn json_response(status: StatusCode, payload: Value, origin: Option<&HeaderValue>) -> Response {
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    let mut resp = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(bytes))
        .expect("valid response");
    apply_cors(&mut resp, origin);
    resp
}

fn apply_cors(resp: &mut Response, origin: Option<&HeaderValue>) {
    let headers = resp.headers_mut();
    if let Some(origin) = origin {
        headers.insert("Access-Control-Allow-Origin", origin.clone());
        headers.insert(
            "Access-Control-Allow-Credentials",
            HeaderValue::from_static("true"),
        );
        headers.insert("Vary", HeaderValue::from_static("Origin"));
    }
    headers.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET,POST,OPTIONS"),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static("Content-Type, X-Folia-Cookie"),
    );
}

fn is_allowed_webview_origin(origin: &HeaderValue) -> bool {
    origin
        .to_str()
        .ok()
        .is_some_and(|value| ALLOWED_WEBVIEW_ORIGINS.contains(&value))
}

async fn handle_request(State(state): State<NeteaseApiState>, req: Request<Body>) -> Response {
    let origin = req.headers().get("origin").cloned();

    if origin
        .as_ref()
        .is_some_and(|value| !is_allowed_webview_origin(value))
    {
        return json_response(
            StatusCode::FORBIDDEN,
            json!({ "code": 403, "msg": "origin not allowed" }),
            None,
        );
    }

    if req.method() == Method::OPTIONS {
        let mut resp = Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("Access-Control-Max-Age", HeaderValue::from_static("86400"))
            .body(Body::empty())
            .expect("valid response");
        apply_cors(&mut resp, origin.as_ref());
        return resp;
    }

    if req.method() != Method::GET && req.method() != Method::POST {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({ "code": 405, "msg": "method not allowed" }),
            origin.as_ref(),
        );
    }

    let uri = req.uri().clone();
    let header_cookie = req
        .headers()
        .get("x-folia-cookie")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if uri.to_string().len() > MAX_URI_LEN {
        return json_response(
            StatusCode::URI_TOO_LONG,
            json!({ "code": 414, "msg": "request uri too long" }),
            origin.as_ref(),
        );
    }
    let query_str = uri.query().unwrap_or("").to_string();
    if query_str.len() > MAX_QUERY_LEN {
        return json_response(
            StatusCode::URI_TOO_LONG,
            json!({ "code": 414, "msg": "query string too long" }),
            origin.as_ref(),
        );
    }

    let method = req.method().clone();
    let path = uri.path().to_string();

    let result = tokio::time::timeout(HANDLER_TIMEOUT, async move {
        let mut params = parse_query(&query_str);
        if method == Method::POST {
            let body_bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
                .await
                .unwrap_or_default();
            merge_body_params(&mut params, &body_bytes);
        }
        merge_header_cookie(&mut params, header_cookie);
        process_request(&state, &path, &params).await
    })
    .await;

    match result {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload, origin.as_ref()),
        Ok(Err(err)) => {
            let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::BAD_GATEWAY);
            json_response(
                status,
                json!({ "code": err.status, "msg": err.msg }),
                origin.as_ref(),
            )
        }
        Err(_) => json_response(
            StatusCode::GATEWAY_TIMEOUT,
            json!({ "code": 504, "msg": "request timed out" }),
            origin.as_ref(),
        ),
    }
}

fn merge_body_params(params: &mut HashMap<String, String>, body: &[u8]) {
    let text = String::from_utf8_lossy(body);
    if text.trim_start().starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
            for (key, value) in map {
                let value = match value {
                    Value::String(s) => s,
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                params.entry(key).or_insert(value);
            }
        }
    } else {
        for (key, value) in url::form_urlencoded::parse(body) {
            params
                .entry(key.into_owned())
                .or_insert_with(|| value.into_owned());
        }
    }
}

fn merge_header_cookie(params: &mut HashMap<String, String>, cookie: Option<String>) {
    if let Some(cookie) = cookie {
        // Keep real sessions out of the request URI; Windows/WebView2 has a
        // much smaller practical URI budget than a full Netease cookie needs.
        params.insert("cookie".to_string(), cookie);
    }
}

// ---------------------------------------------------------------------------
// Request processing
// ---------------------------------------------------------------------------

struct ForwardError {
    status: u16,
    msg: String,
}

struct UpstreamResponse {
    body: Value,
    set_cookies: Vec<String>,
}

async fn process_request(
    state: &NeteaseApiState,
    path: &str,
    params: &HashMap<String, String>,
) -> Result<Value, ForwardError> {
    let route = match find_route(path) {
        Some(route) => route,
        None => {
            return Err(ForwardError {
                status: 404,
                msg: format!("unknown route {path}"),
            })
        }
    };

    if route.protocol == Protocol::Xeapi {
        return Err(ForwardError {
            status: 501,
            msg: format!(
                "XEAPI-only route `{path}` is not implemented in M3; no anonymous registration, please log in"
            ),
        });
    }

    if route.kind == RouteKind::QrCreate {
        return Ok(handle_qr_create(params));
    }

    // Cookie: the frontend passes it via `?cookie=`; fall back to the last
    // known persisted session so requests without it still work.
    let incoming_cookie = params.get("cookie").cloned().unwrap_or_default();
    if !incoming_cookie.is_empty() {
        state.persist_cookie(incoming_cookie.clone());
    }
    let req_cookie = if incoming_cookie.is_empty() {
        state.persisted_cookie().unwrap_or_default()
    } else {
        incoming_cookie
    };

    if route.kind == RouteKind::TrackAll {
        return handle_track_all(state, route, params, &req_cookie).await;
    }

    let upstream_path = resolve_upstream_path(route, params);
    let data = build_data(route.name, params).ok_or_else(|| ForwardError {
        status: 400,
        msg: format!("missing required params for {path}"),
    })?;

    let (body, set_cookies) = match route.protocol {
        Protocol::Weapi => {
            let random_cnip = params
                .get("randomCNIP")
                .map(|s| s == "true")
                .unwrap_or(false);
            let resp = weapi_forward(
                &state.inner.client,
                &upstream_path,
                data,
                &req_cookie,
                random_cnip,
            )
            .await?;
            (resp.body, resp.set_cookies)
        }
        Protocol::Eapi => {
            let resp = eapi_forward(
                &state.inner.client,
                &upstream_path,
                data,
                &req_cookie,
                params
                    .get("randomCNIP")
                    .map(|s| s == "true")
                    .unwrap_or(false),
            )
            .await?;
            (resp.body, resp.set_cookies)
        }
        Protocol::Xeapi => unreachable!("handled above"),
    };

    state.merge_upstream_cookies(&set_cookies);

    let mut body = body;
    normalize_code(&mut body);
    // Return normalized cookie pairs rather than raw Set-Cookie attributes.
    let response_cookie = if set_cookies.is_empty() {
        String::new()
    } else {
        state.persisted_cookie().unwrap_or_default()
    };

    let final_body = match route.kind {
        RouteKind::QrKey => json!({ "data": body, "code": 200 }),
        RouteKind::QrCheck => {
            if response_cookie.is_empty() {
                body
            } else {
                insert_cookie(body, &response_cookie)
            }
        }
        RouteKind::LoginStatus => {
            let mut wrapper = json!({ "data": body });
            if !response_cookie.is_empty() {
                wrapper["cookie"] = json!(response_cookie);
            }
            wrapper
        }
        _ => body,
    };

    Ok(final_body)
}

/// Substitute `{id}` in the upstream path for `/album`, `/artist/album`, and
/// swap sub/unsub paths for `/album/sub` and `/playlist/subscribe`.
fn resolve_upstream_path(route: &Route, params: &HashMap<String, String>) -> String {
    let mut path = route.upstream.to_string();
    if route.id_in_path {
        let id = params.get("id").cloned().unwrap_or_default();
        path = path.replace("{id}", &id);
    }
    match route.name {
        "/album/sub" if params.get("t").map(|s| s != "1").unwrap_or(true) => {
            path = "/api/album/unsub".to_string();
        }
        "/playlist/subscribe" if params.get("t").map(|s| s != "1").unwrap_or(true) => {
            path = "/api/playlist/unsubscribe".to_string();
        }
        _ => {}
    }
    path
}

async fn handle_track_all(
    state: &NeteaseApiState,
    route: &Route,
    params: &HashMap<String, String>,
    req_cookie: &str,
) -> Result<Value, ForwardError> {
    let detail_data =
        json!({ "id": params.get("id").cloned().unwrap_or_default(), "n": 100000, "s": 8 });
    let detail = weapi_forward(
        &state.inner.client,
        route.upstream,
        detail_data,
        req_cookie,
        false,
    )
    .await?;

    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1000);
    let offset = params
        .get("offset")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    let track_ids = detail
        .body
        .pointer("/playlist/trackIds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.get("id")
                        .and_then(Value::as_i64)
                        .map(|id| id.to_string())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if track_ids.is_empty() || offset >= track_ids.len() {
        return Ok(json!({ "code": 200, "songs": [], "privileges": [] }));
    }

    let slice: Vec<String> = track_ids.iter().skip(offset).take(limit).cloned().collect();
    let songs_data = json!({ "c": build_song_c(&slice.join(",")) });
    let songs = weapi_forward(
        &state.inner.client,
        "/api/v3/song/detail",
        songs_data,
        req_cookie,
        false,
    )
    .await?;
    state.merge_upstream_cookies(&songs.set_cookies);
    Ok(songs.body)
}

fn handle_qr_create(params: &HashMap<String, String>) -> Value {
    let key = params.get("key").cloned().unwrap_or_default();
    let qrimg = params
        .get("qrimg")
        .map(|s| s == "true" || s == "1")
        .unwrap_or(false);
    let qrurl = format!("https://music.163.com/login?codekey={key}");
    let mut data = json!({ "qrurl": qrurl, "qrimg": "" });
    if qrimg {
        let svg = render_qr_svg(&qrurl);
        let encoded = base64::engine::general_purpose::STANDARD.encode(svg.as_bytes());
        data["qrimg"] = json!(format!("data:image/svg+xml;base64,{encoded}"));
    }
    json!({ "code": 200, "data": data })
}

fn render_qr_svg(content: &str) -> String {
    let code = qrcode::QrCode::new(content.as_bytes()).expect("qr code generation failed");
    code.render::<qrcode::render::svg::Color>().build()
}

fn normalize_code(body: &mut Value) {
    if let Some(code) = body.get("code") {
        if let Some(text) = code.as_str() {
            if let Ok(num) = text.parse::<i64>() {
                body["code"] = json!(num);
            }
        }
    }
}

fn insert_cookie(mut body: Value, cookie: &str) -> Value {
    body["cookie"] = json!(cookie);
    body
}

// ---------------------------------------------------------------------------
// Upstream forwarding (weapi / eapi)
// ---------------------------------------------------------------------------

async fn weapi_forward(
    client: &reqwest::Client,
    upstream_path: &str,
    data: Value,
    cookie: &str,
    random_cnip: bool,
) -> Result<UpstreamResponse, ForwardError> {
    let cookie_map = parse_cookie_map(cookie);
    let mut data = data;
    if let Value::Object(ref mut map) = data {
        map.insert("e_r".to_string(), json!(false));
        map.insert(
            "csrf_token".to_string(),
            json!(cookie_map.get("__csrf").cloned().unwrap_or_default()),
        );
    }
    let text = serde_json::to_string(&data).map_err(|e| ForwardError {
        status: 500,
        msg: format!("serialize request body: {e}"),
    })?;
    let encrypted = weapi_encrypt(&text, &random_secret_key());
    let url = build_upstream_url(DOMAIN, upstream_path);
    forward_form(
        client,
        &url,
        WEAPI_UA,
        DOMAIN,
        &augment_cookie(&cookie_map),
        &[
            ("params", &encrypted.params),
            ("encSecKey", &encrypted.enc_sec_key),
        ],
        random_cnip,
    )
    .await
}

async fn eapi_forward(
    client: &reqwest::Client,
    upstream_path: &str,
    data: Value,
    cookie: &str,
    random_cnip: bool,
) -> Result<UpstreamResponse, ForwardError> {
    let cookie_map = parse_cookie_map(cookie);
    let now = now_ms();
    let request_id = format!("{}_{:04}", now, rand::thread_rng().gen_range(0..10000));
    let header = eapi_header(&cookie_map, &request_id, now);

    let mut data = data;
    if let Value::Object(ref mut map) = data {
        map.insert("e_r".to_string(), json!(false));
        map.insert("header".to_string(), header.clone());
    }
    let text = serde_json::to_string(&data).map_err(|e| ForwardError {
        status: 500,
        msg: format!("serialize request body: {e}"),
    })?;
    let params = eapi_encrypt(upstream_path, &text);
    let url = build_upstream_url(API_DOMAIN, upstream_path);
    let cookie_header = eapi_cookie_header(&cookie_map, &header);
    forward_form(
        client,
        &url,
        WEAPI_UA,
        DOMAIN,
        &cookie_header,
        &[("params", &params)],
        random_cnip,
    )
    .await
}

async fn forward_form(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    referer: &str,
    cookie: &str,
    fields: &[(&str, &str)],
    random_cnip: bool,
) -> Result<UpstreamResponse, ForwardError> {
    let body = fields
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                uri_component_encode(key),
                uri_component_encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    let mut request = client
        .post(url)
        .header("User-Agent", user_agent)
        .header("Referer", referer)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body);
    if !cookie.is_empty() {
        request = request.header("Cookie", cookie);
    }
    if random_cnip {
        let ip = random_cn_ip();
        request = request
            .header("X-Real-IP", &ip)
            .header("X-Forwarded-For", &ip);
    }

    let response = request.send().await.map_err(|e| ForwardError {
        status: if e.is_timeout() { 504 } else { 502 },
        msg: format!("upstream request failed: {e}"),
    })?;

    let set_cookies = extract_set_cookies(response.headers());
    let bytes = response.bytes().await.map_err(|e| ForwardError {
        status: 502,
        msg: format!("upstream body read failed: {e}"),
    })?;

    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(_) => {
            // eapi transports may still return an encrypted body (e.g. when
            // the upstream overrides e_r); decrypt defensively.
            if let Ok(hex_body) = String::from_utf8(bytes.to_vec()) {
                if let Ok(text) = eapi_res_decrypt(&hex_body) {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        return Ok(UpstreamResponse {
                            body: value,
                            set_cookies,
                        });
                    }
                }
            }
            let snippet = String::from_utf8_lossy(&bytes);
            return Err(ForwardError {
                status: 502,
                msg: format!(
                    "upstream returned non-JSON body: {}",
                    snippet.chars().take(120).collect::<String>()
                ),
            });
        }
    };

    Ok(UpstreamResponse { body, set_cookies })
}

fn build_upstream_url(base: &str, upstream_path: &str) -> String {
    let api_path = upstream_path.trim_start_matches("/api/");
    if base == API_DOMAIN {
        format!("{base}/eapi/{api_path}")
    } else {
        format!("{base}/weapi/{api_path}")
    }
}

fn extract_set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("set-cookie")
        .iter()
        .map(|v| strip_cookie_domain(v.to_str().unwrap_or_default()))
        .collect()
}

fn strip_cookie_domain(cookie: &str) -> String {
    cookie
        .split(';')
        .filter(|part| {
            !part
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("domain=")
        })
        .collect::<Vec<_>>()
        .join(";")
}

// ---------------------------------------------------------------------------
// Cookie handling
// ---------------------------------------------------------------------------

fn parse_cookie_map(cookie: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in cookie.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

fn parse_cookie_pairs(cookie: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for pair in cookie.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=') {
            pairs.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    pairs
}

/// Mirror `processCookieObject` from the reference `util/request.js`: merge
/// the user's cookie with the desktop-app markers Netease expects.
fn augment_cookie(cookie_map: &HashMap<String, String>) -> String {
    let now = now_ms().to_string();
    let nuid = random_hex(32);
    let wnmcid = format!("{}.{}.01.0", random_alpha(6), now);
    let mut pairs: Vec<(String, String)> = cookie_map
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    upsert_cookie(&mut pairs, "__remember_me", "true".to_string());
    upsert_cookie(&mut pairs, "ntes_kaola_ad", "1".to_string());
    upsert_cookie(&mut pairs, "WEVNSM", "1.0.0".to_string());
    if !has_key(&pairs, "_ntes_nuid") {
        upsert_cookie(&mut pairs, "_ntes_nuid", nuid.clone());
    }
    if !has_key(&pairs, "_ntes_nnid") {
        upsert_cookie(&mut pairs, "_ntes_nnid", format!("{nuid},{now}"));
    }
    if !has_key(&pairs, "WNMCID") {
        upsert_cookie(&mut pairs, "WNMCID", wnmcid);
    }
    if !has_key(&pairs, "osver") {
        upsert_cookie(&mut pairs, "osver", OS_OSVER.to_string());
    }
    if !has_key(&pairs, "os") {
        upsert_cookie(&mut pairs, "os", OS_OS.to_string());
    }
    if !has_key(&pairs, "channel") {
        upsert_cookie(&mut pairs, "channel", OS_CHANNEL.to_string());
    }
    if !has_key(&pairs, "appver") {
        upsert_cookie(&mut pairs, "appver", OS_APPVER.to_string());
    }
    if !has_key(&pairs, "NMTID") {
        upsert_cookie(&mut pairs, "NMTID", random_hex(16));
    }

    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", uri_component_encode(k), uri_component_encode(v)))
        .collect::<Vec<_>>()
        .join("; ")
}

fn upsert_cookie(pairs: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = pairs.iter_mut().find(|(k, _)| k == key) {
        *existing = value;
    } else {
        pairs.push((key.to_string(), value));
    }
}

fn has_key(pairs: &[(String, String)], key: &str) -> bool {
    pairs.iter().any(|(k, _)| k == key)
}

fn eapi_header(cookie_map: &HashMap<String, String>, request_id: &str, now: i64) -> Value {
    json!({
        "osver": cookie_map.get("osver").cloned().unwrap_or_else(|| OS_OSVER.to_string()),
        "deviceId": cookie_map.get("deviceId").cloned().unwrap_or_default(),
        "os": cookie_map.get("os").cloned().unwrap_or_else(|| OS_OS.to_string()),
        "appver": cookie_map.get("appver").cloned().unwrap_or_else(|| OS_APPVER.to_string()),
        "versioncode": cookie_map.get("versioncode").cloned().unwrap_or_else(|| "140".to_string()),
        "mobilename": cookie_map.get("mobilename").cloned().unwrap_or_default(),
        "buildver": cookie_map.get("buildver").cloned().unwrap_or_else(|| now.to_string()[..10].to_string()),
        "resolution": cookie_map.get("resolution").cloned().unwrap_or_else(|| "1920x1080".to_string()),
        "__csrf": cookie_map.get("__csrf").cloned().unwrap_or_default(),
        "channel": cookie_map.get("channel").cloned().unwrap_or_else(|| OS_CHANNEL.to_string()),
        "requestId": request_id.to_string(),
    })
}

/// eapi/api requests send the header fields (plus MUSIC_U / MUSIC_A) as the
/// Cookie header, mirroring `createHeaderCookie` in the reference.
fn eapi_cookie_header(cookie_map: &HashMap<String, String>, header: &Value) -> String {
    let mut parts = Vec::new();
    if let Value::Object(map) = header {
        for key in [
            "osver",
            "deviceId",
            "os",
            "appver",
            "versioncode",
            "mobilename",
            "buildver",
            "resolution",
            "__csrf",
            "channel",
            "requestId",
        ] {
            if let Some(Value::String(value)) = map.get(key) {
                if !value.is_empty() {
                    parts.push(format!(
                        "{}={}",
                        uri_component_encode(key),
                        uri_component_encode(value)
                    ));
                }
            }
        }
    }
    for key in ["MUSIC_U", "MUSIC_A"] {
        if let Some(value) = cookie_map.get(key) {
            parts.push(format!(
                "{}={}",
                uri_component_encode(key),
                uri_component_encode(value)
            ));
        }
    }
    parts.join("; ")
}

/// Merge upstream `Set-Cookie` values into the persisted session cookie,
/// replacing same-name keys in place.
fn merge_cookies(current: Option<&str>, set_cookies: &[String]) -> String {
    let mut pairs = current.map(parse_cookie_pairs).unwrap_or_default();
    for set_cookie in set_cookies {
        let cleaned = set_cookie
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if let Some((key, value)) = cleaned.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if let Some((_, existing)) = pairs.iter_mut().find(|(k, _)| *k == key) {
                *existing = value;
            } else {
                pairs.push((key, value));
            }
        }
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn random_hex(len: usize) -> String {
    let mut rng = rand::thread_rng();
    let hex_chars = b"0123456789abcdef";
    (0..len)
        .map(|_| hex_chars[(rng.next_u64() % 16) as usize] as char)
        .collect()
}

fn random_alpha(len: usize) -> String {
    let mut rng = rand::thread_rng();
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|_| CHARS[(rng.next_u64() % 26) as usize] as char)
        .collect()
}

fn random_cn_ip() -> String {
    let mut rng = rand::thread_rng();
    format!(
        "116.{}.{}.{}",
        rng.gen_range(25..=94),
        rng.gen_range(1..=255),
        rng.gen_range(1..=255)
    )
}

fn uri_component_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        params
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    params
}

// ---------------------------------------------------------------------------
// Request body builders (per reference module files)
// ---------------------------------------------------------------------------

fn build_song_c(ids: &str) -> String {
    let parts: Vec<String> = ids
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{{\"id\":{s}}}"))
        .collect();
    format!("[{}]", parts.join(","))
}

fn build_data(name: &str, p: &HashMap<String, String>) -> Option<Value> {
    let get = |key: &str| p.get(key).map(String::as_str).unwrap_or("");
    let int = |key: &str, default: i64| -> i64 {
        p.get(key)
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(default)
    };
    match name {
        "/login/qr/key" => Some(json!({ "type": 3 })),
        "/login/qr/check" => Some(json!({ "key": get("key"), "type": 3 })),
        "/login/status" | "/user/account" | "/logout" | "/personal_fm" => Some(json!({})),
        "/like" => Some(json!({
            "alg": "itembased",
            "trackId": get("id"),
            "like": get("like") != "false",
            "time": "3",
        })),
        "/likelist" => Some(json!({ "uid": get("uid") })),
        "/user/playlist" => Some(json!({
            "uid": get("uid"),
            "limit": int("limit", 30),
            "offset": int("offset", 0),
            "includeVideo": true,
        })),
        "/user/cloud" => Some(json!({
            "limit": int("limit", 30),
            "offset": int("offset", 0),
        })),
        "/user/cloud/detail" => {
            let ids: Vec<String> = get("id")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Some(json!({ "songIds": ids }))
        }
        "/cloud/lyric/get" => Some(json!({
            "userId": get("uid"),
            "songId": get("sid"),
            "lv": -1,
            "kv": -1,
        })),
        "/playlist/detail" => Some(json!({ "id": get("id"), "n": 100000, "s": 8 })),
        "/playlist/tracks" => {
            let tracks: Vec<String> = get("tracks")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Some(json!({
                "op": get("op"),
                "pid": get("pid"),
                "trackIds": serde_json::to_string(&tracks).unwrap_or_default(),
                "imme": "true",
            }))
        }
        "/playlist/subscribe" => {
            if get("t") == "1" {
                Some(json!({ "id": get("id"), "checkToken": CHECK_TOKEN }))
            } else {
                Some(json!({ "id": get("id") }))
            }
        }
        "/playlist/detail/dynamic" => Some(json!({ "id": get("id"), "n": 100000, "s": 8 })),
        "/album" | "/album/detail/dynamic" | "/artist/top/song" | "/artist/detail" => {
            Some(json!({ "id": get("id") }))
        }
        "/album/sublist" => Some(json!({
            "limit": int("limit", 25),
            "offset": int("offset", 0),
            "total": true,
        })),
        "/album/sub" => Some(json!({ "id": get("id") })),
        "/artist/album" => Some(json!({
            "limit": int("limit", 30),
            "offset": int("offset", 0),
            "total": true,
        })),
        "/artist/songs" => Some(json!({
            "id": get("id"),
            "private_cloud": "true",
            "work_type": 1,
            "order": if get("order").is_empty() { "hot" } else { get("order") },
            "offset": int("offset", 0),
            "limit": int("limit", 100),
        })),
        "/song/detail" => Some(json!({ "c": build_song_c(get("ids")) })),
        "/song/url/v1" => Some(json!({
            "ids": format!("[{}]", get("id")),
            "level": get("level"),
            "encodeType": "flac",
        })),
        "/song/chorus" => Some(json!({ "ids": format!("[{}]", get("id")) })),
        "/song/copyright/rcmd" => Some(json!({ "songid": get("songid") })),
        "/lyric/new" => Some(json!({
            "id": get("id"),
            "cp": false,
            "tv": 0,
            "lv": 0,
            "rv": 0,
            "kv": 0,
            "yv": 0,
            "ytv": 0,
            "yrv": 0,
        })),
        "/cloudsearch" => Some(json!({
            "s": get("keywords"),
            "type": 1,
            "limit": int("limit", 30),
            "offset": int("offset", 0),
            "total": true,
        })),
        "/recommend/songs" => Some(json!({ "afresh": get("afresh") })),
        "/recommend/songs/dislike" => Some(json!({
            "resId": get("id"),
            "resType": 4,
            "sceneType": 1,
        })),
        "/history/recommend/songs" => Some(json!({})),
        "/history/recommend/songs/detail" => Some(json!({ "date": get("date") })),
        "/personalized" => Some(json!({
            "limit": int("limit", 30),
            "total": true,
            "n": 1000,
        })),
        "/fm_trash" => Some(json!({ "songId": get("id"), "alg": "RT", "time": "25" })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method as HttpMethod;

    fn test_state() -> NeteaseApiState {
        NeteaseApiState::new()
    }

    #[test]
    fn status_semantics() {
        let state = test_state();
        // initial state before start
        assert_eq!(state.port(), 0);
        let status = state.status_json();
        assert_eq!(status["status"], "starting");
        assert!(status["port"].is_null());
        assert!(status["error"].is_null());
        assert_eq!(status["updatedAt"], 0);

        // running state after successful bind
        state.set_status(ServerStatus {
            status: "running".to_string(),
            port: Some(43123),
            error: None,
            updated_at: 12345,
        });
        let status = state.status_json();
        assert_eq!(status["status"], "running");
        assert_eq!(status["port"], 43123);
        assert_eq!(status["updatedAt"], 12345);
        assert!(status.get("error").unwrap().is_null());
        assert_eq!(state.port(), 43123);
    }

    #[test]
    fn cookie_persistence_roundtrip() {
        let state = test_state();
        assert!(state.persisted_cookie().is_none());
        state.persist_cookie("MUSIC_U=abc; __csrf=xyz".to_string());
        assert_eq!(
            state.persisted_cookie().as_deref(),
            Some("MUSIC_U=abc; __csrf=xyz")
        );
    }

    #[test]
    fn merge_upstream_cookies_replaces_and_appends() {
        let current = Some("MUSIC_U=old; __csrf=keep");
        let set = vec![
            "MUSIC_U=new; Domain=.music.163.com; Path=/; Max-Age=7776000".to_string(),
            "NMTID=abc123; Path=/; HttpOnly".to_string(),
        ];
        let merged = merge_cookies(current, &set);
        assert_eq!(merged, "MUSIC_U=new; __csrf=keep; NMTID=abc123");
    }

    #[test]
    fn augment_cookie_adds_markers() {
        let map = parse_cookie_map("MUSIC_U=token");
        let augmented = augment_cookie(&map);
        assert!(augmented.contains("MUSIC_U=token"));
        assert!(augmented.contains("__remember_me=true"));
        assert!(augmented.contains("WEVNSM=1.0.0"));
        assert!(augmented.contains("os=pc"));
        assert!(augmented.contains("channel=netease"));
        assert!(augmented.contains("NMTID="));
        assert!(augmented.contains("_ntes_nuid="));
        assert!(augmented.contains("WNMCID="));
        assert!(augmented.contains("appver="));
        assert!(augmented.contains("osver="));
    }

    #[test]
    fn parse_query_handles_encoded_cookie() {
        let cookie = "MUSIC_U%3Dabcd%2Bef%3D%3D";
        let params = parse_query(&format!("id=1&cookie={cookie}"));
        assert_eq!(params.get("id").map(String::as_str), Some("1"));
        assert_eq!(
            params.get("cookie").map(String::as_str),
            Some("MUSIC_U=abcd+ef==")
        );
    }

    #[test]
    fn build_song_c_joins_ids() {
        assert_eq!(build_song_c("33894312"), r#"[{"id":33894312}]"#);
        assert_eq!(build_song_c("1, 2 ,3"), r#"[{"id":1},{"id":2},{"id":3}]"#);
        assert_eq!(build_song_c(""), "[]");
    }

    #[test]
    fn upstream_url_construction() {
        assert_eq!(
            build_upstream_url(DOMAIN, "/api/v3/song/detail"),
            "https://music.163.com/weapi/v3/song/detail"
        );
        assert_eq!(
            build_upstream_url(API_DOMAIN, "/api/cloud/lyric/get"),
            "https://interface.music.163.com/eapi/cloud/lyric/get"
        );
    }

    #[test]
    fn resolve_upstream_path_substitutions() {
        let mut params = HashMap::new();
        params.insert("id".to_string(), "32311".to_string());
        params.insert("t".to_string(), "1".to_string());
        let album = find_route("/album").unwrap();
        assert_eq!(resolve_upstream_path(album, &params), "/api/v1/album/32311");
        let sub = find_route("/album/sub").unwrap();
        assert_eq!(resolve_upstream_path(sub, &params), "/api/album/sub");
        params.insert("t".to_string(), "2".to_string());
        assert_eq!(resolve_upstream_path(sub, &params), "/api/album/unsub");
        let ps = find_route("/playlist/subscribe").unwrap();
        params.insert("t".to_string(), "1".to_string());
        assert_eq!(
            resolve_upstream_path(ps, &params),
            "/api/playlist/subscribe"
        );
        params.insert("t".to_string(), "2".to_string());
        assert_eq!(
            resolve_upstream_path(ps, &params),
            "/api/playlist/unsubscribe"
        );
    }

    #[test]
    fn random_cn_ip_is_valid() {
        let ip = random_cn_ip();
        let parts: Vec<&str> = ip.split('.').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "116");
        assert!(parts[1].parse::<u16>().unwrap() >= 25);
        assert!(parts[1].parse::<u16>().unwrap() <= 94);
    }

    #[test]
    fn uri_component_encode_matches_encode_uri_component() {
        assert_eq!(uri_component_encode("aBc-_.~"), "aBc-_.~");
        assert_eq!(
            uri_component_encode("MUSIC_U=abcd+ef=="),
            "MUSIC_U%3Dabcd%2Bef%3D%3D"
        );
    }

    #[tokio::test]
    async fn router_serves_501_404_cors_and_qr_create() {
        let state = test_state();
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // CORS preflight
        let allowed_origin = "http://tauri.localhost";
        let res = client
            .request(HttpMethod::OPTIONS, format!("{base}/song/detail"))
            .header("Origin", allowed_origin)
            .header("Access-Control-Request-Headers", "X-Folia-Cookie")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            res.headers()["access-control-allow-origin"]
                .to_str()
                .unwrap(),
            allowed_origin
        );
        assert_eq!(
            res.headers()["access-control-allow-methods"]
                .to_str()
                .unwrap(),
            "GET,POST,OPTIONS"
        );

        // A browser page cannot borrow the persisted Folia session through
        // the loopback server, even if it discovers the random port.
        let res = client
            .request(HttpMethod::OPTIONS, format!("{base}/song/detail"))
            .header("Origin", "https://attacker.example")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res.headers().get("access-control-allow-origin").is_none());

        // XEAPI-only route -> 501, no fake success
        let res = client
            .get(format!("{base}/register/anonimous"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["code"], 501);
        assert!(body["msg"].as_str().unwrap().contains("XEAPI"));

        // unknown route -> 404
        let res = client
            .get(format!("{base}/definitely/not/here"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["code"], 404);

        // local QR create, no qrimg
        let res = client
            .get(format!("{base}/login/qr/create?key=abc123"))
            .send()
            .await
            .unwrap();
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["code"], 200);
        assert_eq!(
            body["data"]["qrurl"],
            "https://music.163.com/login?codekey=abc123"
        );
        assert_eq!(body["data"]["qrimg"], "");

        // qrimg=true renders an SVG data URL
        let res = client
            .get(format!("{base}/login/qr/create?key=abc123&qrimg=true"))
            .send()
            .await
            .unwrap();
        let body: Value = res.json().await.unwrap();
        let img = body["data"]["qrimg"].as_str().unwrap();
        assert!(img.starts_with("data:image/svg+xml;base64,"));

        // non-GET/POST method rejected
        let res = client
            .request(HttpMethod::DELETE, format!("{base}/song/detail"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);

        server.abort();
    }

    #[test]
    fn long_cookie_is_carried_as_a_header_parameter() {
        let cookie = format!("MUSIC_U={}; __csrf=test", "x".repeat(5000));
        let mut params = HashMap::new();
        merge_header_cookie(&mut params, Some(cookie.clone()));
        assert_eq!(params.get("cookie"), Some(&cookie));
    }

    #[test]
    fn all_routes_have_data_builders() {
        let mut params = HashMap::new();
        params.insert("id".to_string(), "1".to_string());
        params.insert("ids".to_string(), "1".to_string());
        params.insert("uid".to_string(), "1".to_string());
        params.insert("sid".to_string(), "1".to_string());
        params.insert("pid".to_string(), "1".to_string());
        params.insert("tracks".to_string(), "1,2".to_string());
        params.insert("key".to_string(), "k".to_string());
        params.insert("keywords".to_string(), "test".to_string());
        params.insert("songid".to_string(), "1".to_string());
        params.insert("date".to_string(), "2024-01-01".to_string());
        params.insert("t".to_string(), "1".to_string());
        for route in routes::ROUTES {
            if route.kind == RouteKind::QrCreate || route.protocol == Protocol::Xeapi {
                continue;
            }
            if route.kind == RouteKind::TrackAll {
                continue;
            }
            assert!(
                build_data(route.name, &params).is_some(),
                "missing data builder for {}",
                route.name
            );
        }
    }

    // Manual live smoke: forwards real requests through the proxy. Run with
    // `cargo test --lib netease::tests::live_forward_smoke -- --ignored --nocapture`
    // with HTTPS_PROXY=http://127.0.0.1:7890 exported.
    //
    // NOTE (measured 2026-08-06): every weapi route returns real, non-empty JSON
    // over reqwest/SChannel + HTTPS. The historical "empty 200" was NOT a TLS
    // ClientHello fingerprint issue — it was a bug in `weapi_encrypt`: the outer
    // AES layer encrypted the raw inner ciphertext bytes instead of the inner
    // base64 string (see `crypto.rs`), so volc-dcdn could not decrypt `params`
    // and answered an empty 200. After fixing the crypto the same payload is
    // accepted over rustls, SChannel, HTTP/1.1, HTTP/2, HTTPS:443 and HTTP:80.
    #[ignore]
    #[tokio::test]
    async fn live_forward_smoke() {
        let state = test_state();
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        let routes = [
            "/song/detail?ids=33894312",
            "/song/url/v1?id=33894312&level=standard&randomCNIP=true&https=true",
            "/lyric/new?id=33894312",
            "/cloudsearch?keywords=hello&limit=2",
            "/personalized?limit=2",
            "/login/qr/key",
            "/login/qr/check?key=badkey",
            "/likelist?uid=1",
            "/song/copyright/rcmd?songid=33894312",
            "/playlist/detail?id=3778678",
            "/playlist/track/all?id=3778678&limit=3&offset=0",
            "/cloud/lyric/get?uid=1&sid=33894312",
            "/album?id=32311",
        ];
        for route in routes {
            match client.get(format!("{base}{route}")).send().await {
                Ok(res) => {
                    let text = res.text().await.unwrap_or_default();
                    println!("SMOKE {route} -> body={text}");
                }
                Err(e) => println!("SMOKE {route} -> ERROR {e}"),
            }
        }

        // Representative endpoints must return non-empty, valid JSON.
        for route in [
            "/song/detail?ids=33894312",
            "/cloudsearch?keywords=hello&limit=2",
        ] {
            let res = client
                .get(format!("{base}{route}"))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{route}: request failed: {e}"));
            assert_eq!(res.status(), StatusCode::OK, "{route}: http status");
            let text = res
                .text()
                .await
                .unwrap_or_else(|e| panic!("{route}: read body: {e}"));
            assert!(!text.trim().is_empty(), "{route}: empty upstream body");
            let body: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{route}: non-JSON body: {text:?}: {e}"));
            assert!(
                body.get("songs")
                    .and_then(Value::as_array)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
                    || body.get("result").is_some(),
                "{route}: no song/result payload: {text}"
            );
            println!("SMOKE {route} -> OK (valid non-empty JSON)");
        }

        server.abort();
    }
}
