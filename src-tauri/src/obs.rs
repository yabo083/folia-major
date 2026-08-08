//! M7 OBS browser source: authenticated loopback HTTP server that delivers the
//! stage-client assets and live `config` / `clock` / `audio` events to OBS
//! browser-source clients over Server-Sent Events.
//!
//! Behavior mirrors `folia-major/electron/main.cjs` (the OBS browser source
//! section). The server binds `127.0.0.1:configured_port` only while the
//! setting is enabled, tracks the connected client count, and publishes the
//! `obs-browser-source-status-changed` event to the main window.
//!
//! Security: loopback-only bind, query-token auth with constant-time
//! comparison, static asset path sanitization, and a managed runtime kept
//! alive in Tauri state.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Method, Request, Response, StatusCode};
use axum::Router;
use base64::Engine as _;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio_stream::StreamExt as _;

use crate::settings::SettingsStore;

pub const OBS_BROWSER_SOURCE_ENABLED_KEY: &str = "OBS_BROWSER_SOURCE_ENABLED";
pub const OBS_BROWSER_SOURCE_TOKEN_KEY: &str = "OBS_BROWSER_SOURCE_TOKEN";
pub const OBS_BROWSER_SOURCE_PORT_KEY: &str = "OBS_BROWSER_SOURCE_PORT";
pub const DEFAULT_OBS_BROWSER_SOURCE_PORT: u16 = 32108;

const OBS_MAX_BODY_BYTES: usize = 1024 * 1024;

type EmitFn = Arc<dyn Fn(&str, Value) + Send + Sync>;

#[derive(Clone)]
pub struct ObsBrowserSourceState {
    core: Arc<ObsInner>,
}

struct ObsInner {
    emit: EmitFn,
    app: Option<AppHandle>,
    enabled: AtomicBool,
    token: RwLock<Option<String>>,
    port: AtomicU16,
    latest_config: RwLock<Option<Value>>,
    latest_clock: RwLock<Option<Value>>,
    latest_audio: RwLock<Option<Value>>,
    clients: AtomicUsize,
    events_tx: tokio::sync::broadcast::Sender<String>,
    server: Mutex<Option<ObsServer>>,
}

impl ObsBrowserSourceState {
    /// Production constructor. Desktop-only: the state is only managed on
    /// desktop targets (mobile has no OBS browser source integration).
    #[cfg(desktop)]
    pub fn new(app: &AppHandle) -> Self {
        let emit_app = app.clone();
        let emit = Arc::new(move |channel: &str, payload: Value| {
            let _ = emit_app.emit_to("main", channel, payload);
        });
        let (events_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            core: Arc::new(ObsInner {
                emit,
                app: Some(app.clone()),
                enabled: AtomicBool::new(false),
                token: RwLock::new(None),
                port: AtomicU16::new(DEFAULT_OBS_BROWSER_SOURCE_PORT),
                latest_config: RwLock::new(None),
                latest_clock: RwLock::new(None),
                latest_audio: RwLock::new(None),
                clients: AtomicUsize::new(0),
                events_tx,
                server: Mutex::new(None),
            }),
        }
    }

    /// Test constructor (no app, injectable status events).
    #[cfg(test)]
    pub fn for_test(emit: EmitFn) -> Self {
        let (events_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            core: Arc::new(ObsInner {
                emit,
                app: None,
                enabled: AtomicBool::new(false),
                token: RwLock::new(None),
                port: AtomicU16::new(DEFAULT_OBS_BROWSER_SOURCE_PORT),
                latest_config: RwLock::new(None),
                latest_clock: RwLock::new(None),
                latest_audio: RwLock::new(None),
                clients: AtomicUsize::new(0),
                events_tx,
                server: Mutex::new(None),
            }),
        }
    }

    // -- settings cache -------------------------------------------------------

    fn sync_from_settings(&self, settings: &SettingsStore) {
        self.core.enabled.store(
            settings.get_bool(OBS_BROWSER_SOURCE_ENABLED_KEY),
            Ordering::SeqCst,
        );
        let token = settings
            .get(OBS_BROWSER_SOURCE_TOKEN_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.trim().is_empty());
        *self.core.token.write().unwrap() = token;
        let port = settings
            .get(OBS_BROWSER_SOURCE_PORT_KEY)
            .and_then(|v| v.as_u64())
            .and_then(|n| u16::try_from(n).ok())
            .filter(|p| *p > 0)
            .unwrap_or(DEFAULT_OBS_BROWSER_SOURCE_PORT);
        self.core.port.store(port, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.core.enabled.load(Ordering::SeqCst)
    }

    pub fn port(&self) -> u16 {
        self.core.port.load(Ordering::SeqCst)
    }

    fn token(&self) -> Option<String> {
        self.core.token.read().unwrap().clone()
    }

    fn ensure_token(&self, settings: &SettingsStore) -> Result<(), String> {
        if self.token().is_some() {
            return Ok(());
        }
        let token = generate_token();
        settings.set(
            OBS_BROWSER_SOURCE_TOKEN_KEY.to_string(),
            json!(token.clone()),
        )?;
        *self.core.token.write().unwrap() = Some(token);
        Ok(())
    }

    fn client_count(&self) -> usize {
        self.core.clients.load(Ordering::Relaxed)
    }

    // -- status ---------------------------------------------------------------

    pub fn build_status(&self) -> Value {
        let token = self.token();
        let url = token.as_ref().map(|t| {
            format!(
                "http://127.0.0.1:{}/obs?obs=1&token={}",
                self.port(),
                percent_encode(t)
            )
        });
        json!({
            "enabled": self.is_enabled(),
            "port": self.port(),
            "token": token,
            "url": url,
            "clientCount": self.client_count(),
        })
    }

    fn broadcast_status(&self) {
        let payload = self.build_status();
        (self.core.emit)("obs-browser-source-status-changed", payload);
    }

    // -- server lifecycle -----------------------------------------------------

    /// Sync cached settings and start/stop the loopback server.
    /// Desktop-only: invoked from the desktop setup path in lib.rs.
    #[cfg(desktop)]
    pub fn sync_and_serve(&self, app: &AppHandle) -> Result<(), String> {
        let settings = app.state::<SettingsStore>();
        self.sync_from_settings(&settings);
        self.apply_server_state(&settings)?;
        self.broadcast_status();
        Ok(())
    }

    /// Enable/disable the OBS browser source and persist the setting.
    pub fn set_enabled(&self, enabled: bool, settings: &SettingsStore) -> Result<Value, String> {
        settings.set(OBS_BROWSER_SOURCE_ENABLED_KEY.to_string(), json!(enabled))?;
        self.sync_from_settings(settings);
        self.apply_server_state(settings)?;
        self.broadcast_status();
        Ok(self.build_status())
    }

    fn apply_server_state(&self, settings: &SettingsStore) -> Result<(), String> {
        if self.is_enabled() {
            self.ensure_token(settings)?;
            self.start_server()?;
        } else {
            self.stop_server();
        }
        Ok(())
    }

    fn start_server(&self) -> Result<(), String> {
        let mut guard = self.core.server.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let server = ObsServer::start(self.clone()).map_err(|e| {
            eprintln!("[obs] server start failed: {e}");
            e
        })?;
        *guard = Some(server);
        Ok(())
    }

    fn stop_server(&self) {
        *self.core.server.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    // -- publish commands -----------------------------------------------------

    fn publish_event(&self, name: &str, value: Value) {
        if value.is_null() {
            return;
        }
        let frame = sse_frame(name, &value);
        let _ = self.core.events_tx.send(frame);
    }

    pub fn publish_config(&self, config: &Value) -> bool {
        *self.core.latest_config.write().unwrap() = if config.is_null() {
            None
        } else {
            Some(config.clone())
        };
        if !config.is_null() {
            self.publish_event("config", config.clone());
        }
        true
    }

    pub fn publish_clock(&self, clock: &Value) -> bool {
        *self.core.latest_clock.write().unwrap() = if clock.is_null() {
            None
        } else {
            Some(clock.clone())
        };
        if !clock.is_null() {
            self.publish_event("clock", clock.clone());
        }
        true
    }

    pub fn publish_audio(&self, audio: &Value) -> bool {
        *self.core.latest_audio.write().unwrap() = if audio.is_null() {
            None
        } else {
            Some(audio.clone())
        };
        if !audio.is_null() {
            self.publish_event("audio", audio.clone());
        }
        true
    }

    pub fn regenerate_token(&self, settings: &SettingsStore) -> Result<Value, String> {
        let token = generate_token();
        settings.set(
            OBS_BROWSER_SOURCE_TOKEN_KEY.to_string(),
            json!(token.clone()),
        )?;
        *self.core.token.write().unwrap() = Some(token);
        self.broadcast_status();
        Ok(self.build_status())
    }

    // -- SSE / static serving -------------------------------------------------

    fn bootstrap_frames(&self) -> Vec<String> {
        let mut frames = Vec::new();
        if let Some(c) = self.core.latest_config.read().unwrap().clone() {
            frames.push(sse_frame("config", &c));
        }
        if let Some(c) = self.core.latest_clock.read().unwrap().clone() {
            frames.push(sse_frame("clock", &c));
        }
        if let Some(a) = self.core.latest_audio.read().unwrap().clone() {
            frames.push(sse_frame("audio", &a));
        }
        frames
    }

    fn handle_events_stream(&self) -> Response<Body> {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        let guard = SseClientGuard::new(self.clone());
        let bootstrap = self.bootstrap_frames();
        let mut events_rx = self.core.events_tx.subscribe();
        tokio::spawn(async move {
            for frame in bootstrap {
                if tx.send(frame).await.is_err() {
                    drop(guard);
                    return;
                }
            }
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    msg = events_rx.recv() => {
                        match msg {
                            Ok(frame) => {
                                if tx.send(frame).await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
            drop(guard);
        });
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
            .map(|frame| Ok::<_, std::io::Error>(axum::body::Bytes::from(frame)));
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream; charset=utf-8")
            .header("Cache-Control", "no-store")
            .header("Connection", "keep-alive")
            .header("X-Accel-Buffering", "no")
            .header("Access-Control-Allow-Origin", "*")
            .body(Body::from_stream(stream))
            .expect("valid obs sse response")
    }

    fn matches_query_token(&self, query: Option<&str>) -> bool {
        let Some(expected) = self.token() else {
            return false;
        };
        let params = parse_query(query);
        match params.get("token") {
            Some(request_token) => constant_time_token_eq(request_token, &expected),
            None => false,
        }
    }

    fn serve_static(&self, pathname: &str) -> Response<Body> {
        let Some(app) = self.core.app.as_ref() else {
            return obs_json(StatusCode::NOT_FOUND, json!({ "error": "Not found." }));
        };
        let normalized = if pathname == "/" || pathname == "/obs" {
            "index.html".to_string()
        } else {
            pathname.trim_start_matches('/').to_string()
        };
        let decoded = percent_decode(&normalized);
        if decoded.split('/').any(|part| part == "..") {
            return obs_text(StatusCode::FORBIDDEN, "Forbidden");
        }
        match app.asset_resolver().get(decoded.clone()) {
            Some(asset) => {
                let cache_control = if decoded == "index.html" {
                    "no-store"
                } else {
                    "public, max-age=31536000, immutable"
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", asset.mime_type)
                    .header("Cache-Control", cache_control)
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Body::from(asset.bytes))
                    .expect("valid obs asset response")
            }
            None => obs_json(StatusCode::NOT_FOUND, json!({ "error": "Not found." })),
        }
    }

    fn handle_index(&self, query: Option<&str>) -> Response<Body> {
        if cfg!(debug_assertions) {
            if let Some(app) = self.core.app.as_ref() {
                if let Some(mut dev_url) = app.config().build.dev_url.clone() {
                    dev_url.query_pairs_mut().clear();
                    dev_url.query_pairs_mut().append_pair("obs", "1");
                    if let Some(token) = query_token(query) {
                        dev_url.query_pairs_mut().append_pair("token", &token);
                    }
                    dev_url
                        .query_pairs_mut()
                        .append_pair("obsPort", &self.port().to_string());
                    return Response::builder()
                        .status(StatusCode::FOUND)
                        .header("Location", dev_url.to_string())
                        .body(Body::empty())
                        .expect("valid obs redirect");
                }
            }
        }
        self.serve_static("/obs")
    }
}

struct SseClientGuard(ObsBrowserSourceState);

impl SseClientGuard {
    fn new(state: ObsBrowserSourceState) -> Self {
        state.core.clients.fetch_add(1, Ordering::Relaxed);
        state.broadcast_status();
        Self(state)
    }
}

impl Drop for SseClientGuard {
    fn drop(&mut self) {
        self.0.core.clients.fetch_sub(1, Ordering::Relaxed);
        self.0.broadcast_status();
    }
}

// ---------------------------------------------------------------------------
// Router / HTTP handlers
// ---------------------------------------------------------------------------

fn build_router(state: ObsBrowserSourceState) -> Router {
    Router::new()
        .fallback(handle_obs_request)
        .layer(DefaultBodyLimit::max(OBS_MAX_BODY_BYTES))
        .with_state(state)
}

async fn handle_obs_request(
    State(state): State<ObsBrowserSourceState>,
    req: Request<Body>,
) -> Response<Body> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let pathname = uri.path().to_string();
    let query = uri.query().map(|q| q.to_string());

    if pathname == "/obs/health" && method == Method::GET {
        return obs_json(StatusCode::OK, state.build_status());
    }

    if !state.is_enabled() {
        return obs_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "OBS browser source is disabled." }),
        );
    }

    if pathname == "/obs/events" && method == Method::GET {
        if !state.matches_query_token(query.as_deref()) {
            return obs_json(
                StatusCode::UNAUTHORIZED,
                json!({ "error": "Unauthorized." }),
            );
        }
        return state.handle_events_stream();
    }

    if pathname == "/obs" && method == Method::GET {
        if !state.matches_query_token(query.as_deref()) {
            return obs_json(
                StatusCode::UNAUTHORIZED,
                json!({ "error": "Unauthorized." }),
            );
        }
        return state.handle_index(query.as_deref());
    }

    state.serve_static(&pathname)
}

// ---------------------------------------------------------------------------
// Server lifecycle (kept alive in managed state)
// ---------------------------------------------------------------------------

/// Owns the tokio runtime that drives the OBS axum server while enabled.
pub struct ObsServer {
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for ObsServer {
    // Dropping a tokio Runtime blocks and panics in async contexts, so move it
    // to a plain thread; joining keeps the loopback port closed before
    // `set_enabled(false)` returns (the disabled-state tests assert the port is
    // immediately unreachable). The runtime is dedicated: its workers never
    // drop it.
    fn drop(&mut self) {
        if let Some(runtime) = self._runtime.take() {
            std::thread::spawn(move || drop(runtime))
                .join()
                .expect("obs runtime shutdown panicked");
        }
    }
}

impl ObsServer {
    fn start(state: ObsBrowserSourceState) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("failed to create tokio runtime: {e}"))?;
        let port = state.port();
        let router = build_router(state.clone());
        runtime
            .block_on(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                    .await
                    .map_err(|e| format!("failed to bind OBS server on 127.0.0.1:{port}: {e}"))?;
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, router).await {
                        eprintln!("[obs] server stopped: {e}");
                    }
                });
                Ok::<(), String>(())
            })
            .map_err(|e| e.to_string())?;
        eprintln!("[obs] browser source listening on http://127.0.0.1:{port}");
        Ok(Self {
            _runtime: Some(runtime),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sse_frame(name: &str, payload: &Value) -> String {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string())
    )
}

fn parse_query(query: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    if let Some(query) = query {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            params
                .entry(key.into_owned())
                .or_insert_with(|| value.into_owned());
        }
    }
    params
}

fn query_token(query: Option<&str>) -> Option<String> {
    parse_query(query)
        .get("token")
        .cloned()
        .filter(|t| !t.is_empty())
}

fn constant_time_token_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    let mut diff = 0u8;
    for (x, y) in ha.iter().zip(hb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn obs_json(status: StatusCode, payload: Value) -> Response<Body> {
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from(bytes))
        .expect("valid obs json response")
}

fn obs_text(status: StatusCode, text: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from(text.to_string()))
        .expect("valid obs text response")
}

// ---------------------------------------------------------------------------
// Tauri commands (snake_case, shim-invoked)
// ---------------------------------------------------------------------------

#[tauri::command]
// 返回 OBS 浏览器源状态（enabled/port/token/url/clientCount）。
pub fn obs_browser_source_get_status(
    app: AppHandle,
    state: tauri::State<'_, ObsBrowserSourceState>,
) -> Result<Value, String> {
    if state.is_enabled() {
        let settings = app.state::<SettingsStore>();
        state.ensure_token(&settings)?;
    }
    Ok(state.build_status())
}

#[tauri::command]
// 持久化 OBS 浏览器源开关并按状态同步启动/停止回环服务器。
pub fn obs_browser_source_set_enabled(
    app: AppHandle,
    enabled: bool,
    state: tauri::State<'_, ObsBrowserSourceState>,
) -> Result<Value, String> {
    let settings = app.state::<SettingsStore>();
    state.set_enabled(enabled, &settings)
}

#[tauri::command]
// 重新生成 OBS 浏览器源 token（持久化 + 广播状态）。
pub fn obs_browser_source_regenerate_token(
    app: AppHandle,
    state: tauri::State<'_, ObsBrowserSourceState>,
) -> Result<Value, String> {
    let settings = app.state::<SettingsStore>();
    state.regenerate_token(&settings)
}

#[tauri::command]
// 缓存并广播 OBS config 事件。
pub fn obs_browser_source_publish_config(
    config: Value,
    state: tauri::State<'_, ObsBrowserSourceState>,
) -> Result<bool, String> {
    Ok(state.publish_config(&config))
}

#[tauri::command]
// 缓存并广播 OBS clock 事件。
pub fn obs_browser_source_publish_clock(
    clock: Value,
    state: tauri::State<'_, ObsBrowserSourceState>,
) -> Result<bool, String> {
    Ok(state.publish_clock(&clock))
}

#[tauri::command]
// 缓存并广播 OBS audio 事件。
pub fn obs_browser_source_publish_audio(
    audio: Value,
    state: tauri::State<'_, ObsBrowserSourceState>,
) -> Result<bool, String> {
    Ok(state.publish_audio(&audio))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener as StdTcpListener;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn free_port() -> u16 {
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "folia-obs-test-{}-{}",
            std::process::id(),
            rand::thread_rng().next_u32()
        ))
    }

    fn enable_state(state: &ObsBrowserSourceState, root: &Path) -> Value {
        let state = state.clone();
        let root = root.to_path_buf();
        let status = std::thread::spawn(move || {
            let store = SettingsStore::open(&root).unwrap();
            state.set_enabled(true, &store)
        })
        .join()
        .expect("enable thread panicked")
        .expect("enable failed");
        if let Some(port) = status["port"].as_u64() {
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port as u16)).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        status
    }

    #[test]
    fn constant_time_token_eq_is_secure() {
        assert!(constant_time_token_eq("aBc123_-", "aBc123_-"));
        assert!(!constant_time_token_eq("aBc123_-", "aBc123_+"));
        assert!(!constant_time_token_eq("short", "a-longer-token"));
        assert!(!constant_time_token_eq("", "x"));
    }

    #[test]
    fn status_shape_and_url_encoding() {
        let root = temp_root();
        let store = SettingsStore::open(&root).unwrap();
        store
            .set(OBS_BROWSER_SOURCE_PORT_KEY.to_string(), json!(32108))
            .unwrap();
        store
            .set(
                OBS_BROWSER_SOURCE_TOKEN_KEY.to_string(),
                json!("tok_ABC-_123"),
            )
            .unwrap();
        let emit: EmitFn = Arc::new(|_, _| {});
        let state = ObsBrowserSourceState::for_test(emit);
        state.sync_from_settings(&store);
        let status = state.build_status();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["port"], 32108);
        assert_eq!(status["token"], "tok_ABC-_123");
        assert_eq!(status["clientCount"], 0);
        assert_eq!(
            status["url"],
            "http://127.0.0.1:32108/obs?obs=1&token=tok_ABC-_123"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn auth_health_and_disabled_contract() {
        let root = temp_root();
        let store = SettingsStore::open(&root).unwrap();
        let port = free_port();
        store
            .set(OBS_BROWSER_SOURCE_PORT_KEY.to_string(), json!(port))
            .unwrap();
        let emit: EmitFn = Arc::new(|_, _| {});
        let state = ObsBrowserSourceState::for_test(emit);
        let status = enable_state(&state, &root);
        let token = status["token"].as_str().unwrap().to_string();
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // health is unauthenticated
        let res = client
            .get(format!("{base}/obs/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let health: Value = res.json().await.unwrap();
        assert_eq!(health["enabled"], true);
        assert_eq!(health["port"], port);

        // /obs/events requires the token
        let res = client
            .get(format!("{base}/obs/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = client
            .get(format!("{base}/obs/events?token=wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /obs requires the token
        let res = client.get(format!("{base}/obs")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = client
            .get(format!("{base}/obs?token={token}"))
            .send()
            .await
            .unwrap();
        // no embedded assets in tests (no app): falls through to 404
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // static paths are served without a token (but 404 without assets)
        let res = client
            .get(format!("{base}/assets/x.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // path traversal is rejected
        let res = client
            .get(format!("{base}/../secret"))
            .send()
            .await
            .unwrap();
        assert!(res.status().is_server_error() || res.status() == StatusCode::NOT_FOUND);

        // CORS headers present
        let res = client
            .get(format!("{base}/obs/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.headers()["access-control-allow-origin"]
                .to_str()
                .unwrap(),
            "*"
        );

        // disable stops the server
        let store2 = SettingsStore::open(&root).unwrap();
        state.set_enabled(false, &store2).unwrap();
        let res = client.get(format!("{base}/obs/health")).send().await;
        assert!(res.is_err(), "server should be stopped when disabled");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn sse_streams_bootstrap_and_live_events() {
        let root = temp_root();
        let store = SettingsStore::open(&root).unwrap();
        let port = free_port();
        store
            .set(OBS_BROWSER_SOURCE_PORT_KEY.to_string(), json!(port))
            .unwrap();
        let emit: EmitFn = Arc::new(|_, _| {});
        let state = ObsBrowserSourceState::for_test(emit);
        let status = enable_state(&state, &root);
        let token = status["token"].as_str().unwrap().to_string();

        // Publish config/clock/audio before connecting (bootstrap).
        state.publish_config(&json!({ "theme": "dark" }));
        state.publish_clock(&json!({ "currentTime": 12.5 }));
        state.publish_audio(&json!({ "audioPower": 0.5 }));

        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let request = format!(
            "GET /obs/events?token={token} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        socket.write_all(request.as_bytes()).await.unwrap();

        let mut buffer = Vec::new();
        let mut tmp = [0u8; 2048];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if tokio::time::Instant::now() > deadline {
                break;
            }
            let n = tokio::time::timeout(Duration::from_millis(500), socket.read(&mut tmp))
                .await
                .unwrap_or(Ok(0))
                .unwrap_or(0);
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&tmp[..n]);
            if String::from_utf8_lossy(&buffer).contains("event: audio") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buffer).to_string();
        assert!(
            text.contains("event: config"),
            "missing bootstrap config: {text}"
        );
        assert!(
            text.contains("event: clock"),
            "missing bootstrap clock: {text}"
        );
        assert!(
            text.contains("event: audio"),
            "missing bootstrap audio: {text}"
        );
        assert!(
            text.contains("\"theme\":\"dark\""),
            "missing config payload: {text}"
        );
        assert_eq!(state.client_count(), 1, "SSE client should be tracked");

        // Live publish reaches the connected client.
        state.publish_config(&json!({ "theme": "light" }));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if tokio::time::Instant::now() > deadline {
                break;
            }
            let n = tokio::time::timeout(Duration::from_millis(500), socket.read(&mut tmp))
                .await
                .unwrap_or(Ok(0))
                .unwrap_or(0);
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&tmp[..n]);
            if String::from_utf8_lossy(&buffer).contains("\"theme\":\"light\"") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buffer).to_string();
        assert!(
            text.contains("\"theme\":\"light\""),
            "missing live config: {text}"
        );

        // Unauthorized events connection is rejected before streaming.
        let mut bad = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        bad.write_all(
            b"GET /obs/events?token=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let mut bad_resp = Vec::new();
        bad.read_to_end(&mut bad_resp).await.unwrap();
        assert!(String::from_utf8_lossy(&bad_resp).contains("401"));

        drop(socket);
        // Allow the guard to drop and the count to settle.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            state.client_count(),
            0,
            "client count should return to zero"
        );

        let store2 = SettingsStore::open(&root).unwrap();
        state.set_enabled(false, &store2).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }
}
