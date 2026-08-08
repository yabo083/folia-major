//! M7 Stage API: authenticated loopback HTTP/WebSocket server for desktop-local
//! integrations (external tools push parser-compatible lyrics, a media session,
//! or ask Folia to search/play songs).
//!
//! Behavior mirrors `folia-major/electron/stageApi.cjs` (spec source:
//! `folia-major/test/unit/stage/stageApi.test.ts`). The server binds
//! `127.0.0.1:configured_port` only while Stage mode is enabled with the
//! `stage-api` source, and publishes the `stage-session-updated` /
//! `stage-session-cleared` / `stage-external-play-request` /
//! `stage-player-control-request` / `stage-player-queue-request` events.
//!
//! Security: loopback-only bind, bearer token auth with constant-time
//! comparison, request/body limits, session dirs under app_data_dir, and no
//! lock held across an await.

use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, FromRequest, Query, State};
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use base64::Engine as _;
use rand::RngCore;
use serde_json::{json, Value};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use tauri::{AppHandle, Emitter, Manager};

use crate::netease::NeteaseApiState;
use crate::settings::SettingsStore;

// ---------------------------------------------------------------------------
// Constants (mirror stageApi.cjs)
// ---------------------------------------------------------------------------

pub const STAGE_MODE_ENABLED_KEY: &str = "STAGE_MODE_ENABLED";
pub const STAGE_MODE_SOURCE_KEY: &str = "STAGE_MODE_SOURCE";
pub const STAGE_API_TOKEN_KEY: &str = "STAGE_API_TOKEN";
pub const STAGE_API_PORT_KEY: &str = "STAGE_API_PORT";
pub const DEFAULT_STAGE_API_PORT: u16 = 32107;

const STAGE_JSON_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const STAGE_MULTIPART_FIELD_LIMIT_BYTES: u64 = 2 * 1024 * 1024;
const STAGE_MULTIPART_FILE_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;
const STAGE_MULTIPART_FILE_COUNT_LIMIT: usize = 3;
const STAGE_MULTIPART_PART_COUNT_LIMIT: usize = 10;
const STAGE_MULTIPART_FIELD_COUNT_LIMIT: usize = 10;
// Overall request body budget for the multipart session route (files + fields).
const STAGE_MAX_BODY_BYTES: usize = STAGE_MULTIPART_FILE_LIMIT_BYTES as usize + 1024 * 1024;
const STAGE_SESSION_RETENTION_LIMIT: usize = 12;
const STAGE_PLAY_REQUEST_TIMEOUT_MS: u64 = 15_000;
const STAGE_PLAYER_REQUEST_TIMEOUT_MS: u64 = 10_000;
const STAGE_PLAYER_QUEUE_DEFAULT_LIMIT: i64 = 100;
const STAGE_PLAYER_QUEUE_MAX_LIMIT: i64 = 500;

const STAGE_LYRICS_FORMAT_VALUES: &[&str] = &["lrc", "enhanced-lrc", "vtt", "yrc", "qrc"];
const STAGE_PLAYER_CONTEXT_VALUES: &[&str] = &[
    "normal-playback",
    "stage-session",
    "external-playback-source",
];
const STAGE_PLAYER_STATE_VALUES: &[&str] = &["IDLE", "PLAYING", "PAUSED"];
const PLAYER_CONTROL_CAPABILITY: &[(&str, &str)] = &[
    ("next", "next"),
    ("prev", "previous"),
    ("pause", "pause"),
    ("resume", "resume"),
    ("seek", "seek"),
];
const PLAYER_QUEUE_CAPABILITY: &[(&str, &str)] = &[
    ("append", "append"),
    ("insert-next", "insertNext"),
    ("remove", "remove"),
    ("move", "move"),
    ("select", "select"),
    ("clear", "clear"),
];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StageError {
    status: StatusCode,
    code: String,
    message: String,
    details: Option<Value>,
}

impl StageError {
    fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            code: code.to_string(),
            message: message.into(),
            details: None,
        }
    }

    fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    fn validation(message: &str, code: &str) -> Self {
        Self::new(400, code, message)
    }
}

impl IntoResponse for StageError {
    fn into_response(self) -> Response<Body> {
        let mut payload = json!({
            "error": self.message,
            "code": self.code,
        });
        if let Some(details) = self.details {
            payload["details"] = details;
        }
        json_response(self.status, payload)
    }
}

// ---------------------------------------------------------------------------
// Helpers (mirror stageApi.cjs pure helpers)
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn random_uuid() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 16];
    rng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&bytes[0..4]),
        hex::encode(&bytes[4..6]),
        hex::encode(&bytes[6..8]),
        hex::encode(&bytes[8..10]),
        hex::encode(&bytes[10..16])
    )
}

fn random_suffix() -> String {
    format!("{:08x}", rand::thread_rng().next_u32())
}

fn normalize_stage_text(value: &Value) -> String {
    value.as_str().unwrap_or("").trim().to_string()
}

fn normalize_stage_number(value: &Value, fallback: f64) -> f64 {
    value.as_f64().filter(|v| v.is_finite()).unwrap_or(fallback)
}

fn normalize_stage_integer(value: &Value, fallback: i64) -> i64 {
    let n = normalize_stage_number(value, fallback as f64);
    n.floor() as i64
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().map(|i| i != 0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

// JS `x ?? fallback`: fall back only on null/missing.
fn js_nullish_coalesce(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        None => fallback,
        Some(Value::Null) => fallback,
        Some(other) => js_truthy(other),
    }
}

fn is_stage_lyrics_format(value: &str) -> bool {
    STAGE_LYRICS_FORMAT_VALUES.contains(&value)
}

fn take_digits(text: &str) -> (String, String) {
    let mut digits = String::new();
    for (idx, ch) in text.char_indices() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            return (digits, text[idx..].to_string());
        }
    }
    (digits, String::new())
}

// /\d{1,2}:\d{2}(?:[.:]\d{1,3})?] after a `[`
fn scan_lrc_timeline(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let rest = &text[i + 1..];
            let (minutes, after_min) = take_digits(rest);
            if !minutes.is_empty()
                && minutes.len() <= 2
                && after_min.as_bytes().first() == Some(&b':')
            {
                let rest = &after_min[1..];
                let (seconds, after_sec) = take_digits(rest);
                if !seconds.is_empty() && seconds.len() == 2 {
                    if after_sec.as_bytes().first() == Some(&b']') {
                        return true;
                    }
                    if let Some(&b) = after_sec.as_bytes().first() {
                        if b == b'.' || b == b':' {
                            let rest = &after_sec[1..];
                            let (_, after_ms) = take_digits(rest);
                            if after_ms.as_bytes().first() == Some(&b']') {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    false
}

fn has_lrc_timeline(text: &str) -> bool {
    scan_lrc_timeline(text)
}

// /<\d{2}:\d{2}[.:]\d{2,3}>/
fn scan_angled_word_timeline(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let rest = &text[i + 1..];
            let (minutes, after_min) = take_digits(rest);
            if minutes.len() == 2 && after_min.as_bytes().first() == Some(&b':') {
                let rest = &after_min[1..];
                let (seconds, after_sec) = take_digits(rest);
                if seconds.len() == 2 {
                    if let Some(&b) = after_sec.as_bytes().first() {
                        if b == b'.' || b == b':' {
                            let rest = &after_sec[1..];
                            let (ms, after_ms) = take_digits(rest);
                            if (2..=3).contains(&ms.len())
                                && after_ms.as_bytes().first() == Some(&b'>')
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    false
}

// A line carrying two or more `[mm:ss]`-style timelines (word-level LRC).
fn count_bracket_timelines(line: &str) -> usize {
    let mut count = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let tail = &line[i..];
            if scan_lrc_timeline(tail) {
                count += 1;
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b']' {
                    j += 1;
                }
                i = (j + 1).min(bytes.len());
                continue;
            }
        }
        i += 1;
    }
    count
}

fn has_enhanced_word_timeline(text: &str) -> bool {
    if scan_angled_word_timeline(text) {
        return true;
    }
    text.split('\n')
        .any(|line| count_bracket_timelines(line) >= 2)
}

fn detect_stage_lyrics_format(text: &str) -> Option<String> {
    let normalized = text.trim();
    if normalized.is_empty() || !has_lrc_timeline(normalized) {
        return None;
    }
    Some(if has_enhanced_word_timeline(normalized) {
        "enhanced-lrc".to_string()
    } else {
        "lrc".to_string()
    })
}

fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    let mut params = HashMap::new();
    if let Some(query) = query {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            params
                .entry(key.into_owned())
                .or_insert_with(|| value.into_owned());
        }
    }
    params
}

// Constant-time token comparison: both sides are hashed and the digests are
// compared without short-circuiting (avoids content length/timing leaks).
fn constant_time_token_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    let mut diff = 0u8;
    for (x, y) in ha.iter().zip(hb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn bearer_token_from(
    headers: &axum::http::header::HeaderMap,
    query: Option<&str>,
) -> Option<String> {
    if let Some(auth) = headers.get("authorization") {
        if let Ok(auth) = auth.to_str() {
            let auth = auth.trim();
            for prefix in ["Bearer ", "bearer "] {
                if let Some(rest) = auth.strip_prefix(prefix) {
                    let rest = rest.trim();
                    if !rest.is_empty() {
                        return Some(rest.to_string());
                    }
                }
            }
        }
    }
    if let Some(query) = query {
        let params = parse_query(Some(query));
        if let Some(token) = params.get("token") {
            if !token.is_empty() {
                return Some(token.clone());
            }
        }
    }
    None
}

fn json_response(status: StatusCode, payload: Value) -> Response<Body> {
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type",
        )
        .header("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS")
        .body(Body::from(bytes))
        .expect("valid stage json response")
}

fn cors_no_content() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type",
        )
        .header("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS")
        .body(Body::empty())
        .expect("valid stage cors response")
}

// ---------------------------------------------------------------------------
// Pending request bridge (renderer round-trips)
// ---------------------------------------------------------------------------

type PendingResult = Result<Value, StageError>;

struct PendingEntry {
    tx: tokio::sync::oneshot::Sender<PendingResult>,
    timer: tokio::task::JoinHandle<()>,
}

struct PendingStore {
    inner: Mutex<HashMap<String, PendingEntry>>,
}

impl PendingStore {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    // self: &Arc<Self> so the timer task can keep the store alive.
    fn register(
        self: &Arc<Self>,
        request_id: &str,
        timeout_ms: u64,
        timeout_error: StageError,
    ) -> tokio::sync::oneshot::Receiver<PendingResult> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let store = Arc::clone(self);
        let id = request_id.to_string();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
            store.cancel(&id, timeout_error);
        });
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(request_id.to_string(), PendingEntry { tx, timer });
        rx
    }

    fn complete(&self, request_id: &str, result: PendingResult) -> bool {
        let entry = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(request_id);
        match entry {
            Some(entry) => {
                entry.timer.abort();
                let _ = entry.tx.send(result);
                true
            }
            None => false,
        }
    }

    fn cancel(&self, request_id: &str, err: StageError) {
        let entry = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(request_id);
        if let Some(entry) = entry {
            let _ = entry.tx.send(Err(err));
        }
    }

    fn cancel_all(&self, err: StageError) {
        let entries: Vec<_> = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            entry.timer.abort();
            let _ = entry.tx.send(Err(err.clone()));
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type EmitFn = Arc<dyn Fn(&str, Value) + Send + Sync>;
type PortFn = Arc<dyn Fn() -> u16 + Send + Sync>;

#[derive(Clone)]
pub struct StageState {
    core: Arc<StageInner>,
}

struct StageInner {
    emit: EmitFn,
    netease_port: PortFn,
    client: reqwest::Client,
    sessions_root: PathBuf,
    // cached settings
    mode_enabled: AtomicBool,
    source: RwLock<String>,
    token: RwLock<Option<String>>,
    port: AtomicU16,
    // session state
    lyrics_session: Mutex<Option<Value>>,
    media_session: Mutex<Option<Value>>,
    active_entry_kind: Mutex<Option<String>>,
    active_session_id: Mutex<Option<String>>,
    active_audio_path: Mutex<Option<PathBuf>>,
    active_cover_path: Mutex<Option<PathBuf>>,
    session_assets: Mutex<VecDeque<(String, SessionAssets)>>,
    // player snapshot state
    player_snapshot: Mutex<Option<Value>>,
    track_key: Mutex<Option<String>>,
    queue_key: Mutex<Option<String>>,
    playback_key: Mutex<Option<String>>,
    player_events: tokio::sync::broadcast::Sender<String>,
    ws_kick: tokio::sync::broadcast::Sender<()>,
    play_pending: Arc<PendingStore>,
    control_pending: Arc<PendingStore>,
    queue_pending: Arc<PendingStore>,
    server: Mutex<Option<StageApiServer>>,
}

#[derive(Clone)]
struct SessionAssets {
    cover_path: Option<PathBuf>,
    cover_mime_type: Option<String>,
    working_directory: PathBuf,
}

impl StageState {
    /// Production constructor: emits events to the main window and resolves the
    /// Netease search port from the managed Netease API server.
    pub fn new(app: &AppHandle, app_data_dir: PathBuf) -> Self {
        let emit_app = app.clone();
        let emit = Arc::new(move |channel: &str, payload: Value| {
            let _ = emit_app.emit_to("main", channel, payload);
        });
        let port_app = app.clone();
        let netease_port = Arc::new(move || port_app.state::<NeteaseApiState>().port());
        Self::with_components(app_data_dir.join("stage"), emit, netease_port)
    }

    /// Test constructor with injectable emit/search-port behavior.
    #[cfg(test)]
    pub fn for_test(sessions_root: PathBuf, emit: EmitFn, netease_port: PortFn) -> Self {
        Self::with_components(sessions_root, emit, netease_port)
    }

    fn with_components(sessions_root: PathBuf, emit: EmitFn, netease_port: PortFn) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let (player_events, _) = tokio::sync::broadcast::channel(128);
        let (ws_kick, _) = tokio::sync::broadcast::channel(16);
        Self {
            core: Arc::new(StageInner {
                emit,
                netease_port,
                client,
                sessions_root,
                mode_enabled: AtomicBool::new(false),
                source: RwLock::new("stage-api".to_string()),
                token: RwLock::new(None),
                port: AtomicU16::new(DEFAULT_STAGE_API_PORT),
                lyrics_session: Mutex::new(None),
                media_session: Mutex::new(None),
                active_entry_kind: Mutex::new(None),
                active_session_id: Mutex::new(None),
                active_audio_path: Mutex::new(None),
                active_cover_path: Mutex::new(None),
                session_assets: Mutex::new(VecDeque::new()),
                player_snapshot: Mutex::new(None),
                track_key: Mutex::new(None),
                queue_key: Mutex::new(None),
                playback_key: Mutex::new(None),
                player_events,
                ws_kick,
                play_pending: Arc::new(PendingStore::new()),
                control_pending: Arc::new(PendingStore::new()),
                queue_pending: Arc::new(PendingStore::new()),
                server: Mutex::new(None),
            }),
        }
    }

    // -- settings cache -------------------------------------------------------

    fn sync_from_settings(&self, settings: &SettingsStore) {
        self.core
            .mode_enabled
            .store(settings.get_bool(STAGE_MODE_ENABLED_KEY), Ordering::SeqCst);
        let source = settings
            .get(STAGE_MODE_SOURCE_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "stage-api".to_string());
        *self.core.source.write().unwrap() = source;
        let token = settings
            .get(STAGE_API_TOKEN_KEY)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.trim().is_empty());
        *self.core.token.write().unwrap() = token;
        let port = settings
            .get(STAGE_API_PORT_KEY)
            .and_then(|v| v.as_u64())
            .and_then(|n| u16::try_from(n).ok())
            .filter(|p| *p > 0)
            .unwrap_or(DEFAULT_STAGE_API_PORT);
        self.core.port.store(port, Ordering::SeqCst);
    }

    pub fn is_mode_enabled(&self) -> bool {
        self.core.mode_enabled.load(Ordering::SeqCst)
    }

    pub fn stage_source(&self) -> String {
        let source = self.core.source.read().unwrap().clone();
        match source.as_str() {
            "now-playing" | "playercap" => source,
            _ => "stage-api".to_string(),
        }
    }

    pub fn is_stage_enabled(&self) -> bool {
        self.is_mode_enabled() && self.stage_source() == "stage-api"
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
        settings.set(STAGE_API_TOKEN_KEY.to_string(), json!(token.clone()))?;
        *self.core.token.write().unwrap() = Some(token);
        Ok(())
    }

    // -- server lifecycle -----------------------------------------------------

    /// Sync cached settings and start/stop the loopback server (mirrors
    /// `syncStageModeState`; also generates a token when stage-api is active).
    pub fn sync_and_serve(&self, app: &AppHandle) -> Result<Value, String> {
        let settings = app.state::<SettingsStore>();
        self.sync_from_settings(&settings);
        self.apply_server_state(&settings)?;
        self.emit("stage-session-updated", self.build_status());
        Ok(self.build_status())
    }

    /// Enable/disable Stage mode and persist (the `stage-api` source is set as
    /// the default on first enable, matching the reference).
    pub fn set_enabled(&self, enabled: bool, settings: &SettingsStore) -> Result<Value, String> {
        settings.set(STAGE_MODE_ENABLED_KEY.to_string(), json!(enabled))?;
        if enabled && !settings.has(STAGE_MODE_SOURCE_KEY) {
            settings.set(STAGE_MODE_SOURCE_KEY.to_string(), json!("stage-api"))?;
        }
        self.sync_from_settings(settings);
        self.apply_server_state(settings)?;
        self.emit("stage-session-updated", self.build_status());
        Ok(self.build_status())
    }

    fn apply_server_state(&self, settings: &SettingsStore) -> Result<(), String> {
        if self.is_stage_enabled() {
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
        let server = StageApiServer::start(self.clone()).map_err(|e| {
            eprintln!("[stage] server start failed: {e}");
            e
        })?;
        *guard = Some(server);
        Ok(())
    }

    fn stop_server(&self) {
        self.core.play_pending.cancel_all(StageError::new(
            503,
            "STAGE_PLAY_CANCELED",
            "Stage server stopped.",
        ));
        self.core.control_pending.cancel_all(StageError::new(
            503,
            "STAGE_PLAYER_CONTROL_UNAVAILABLE",
            "Stage player requests were canceled.",
        ));
        self.core.queue_pending.cancel_all(StageError::new(
            503,
            "STAGE_PLAYER_QUEUE_UNAVAILABLE",
            "Stage player requests were canceled.",
        ));
        self.clear_player_snapshot();
        self.clear_session_state();
        *self.core.server.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn clear_player_snapshot(&self) {
        *self.core.player_snapshot.lock().unwrap() = None;
        *self.core.track_key.lock().unwrap() = None;
        *self.core.queue_key.lock().unwrap() = None;
        *self.core.playback_key.lock().unwrap() = None;
    }

    fn clear_session_state(&self) {
        *self.core.lyrics_session.lock().unwrap() = None;
        *self.core.media_session.lock().unwrap() = None;
        *self.core.active_entry_kind.lock().unwrap() = None;
        *self.core.active_session_id.lock().unwrap() = None;
        *self.core.active_audio_path.lock().unwrap() = None;
        *self.core.active_cover_path.lock().unwrap() = None;
    }

    fn emit(&self, channel: &str, payload: Value) {
        (self.core.emit)(channel, payload);
    }

    // -- status ---------------------------------------------------------------

    pub fn build_status(&self) -> Value {
        let mode_enabled = self.is_mode_enabled();
        json!({
            "domain": "stage-input",
            "direction": "outside-in",
            "enabled": self.is_stage_enabled(),
            "modeEnabled": mode_enabled,
            "source": if mode_enabled { json!(self.stage_source()) } else { Value::Null },
            "port": self.port(),
            "token": self.token(),
            "activeEntryKind": self.active_entry_kind(),
            "lyricsSession": self.lyrics_session(),
            "mediaSession": self.media_session(),
        })
    }

    fn build_health(&self) -> Value {
        let mode_enabled = self.is_mode_enabled();
        json!({
            "enabled": self.is_stage_enabled(),
            "modeEnabled": mode_enabled,
            "source": if mode_enabled { json!(self.stage_source()) } else { Value::Null },
            "port": self.port(),
            "activeEntryKind": self.active_entry_kind(),
        })
    }

    fn active_entry_kind(&self) -> Option<String> {
        self.core.active_entry_kind.lock().unwrap().clone()
    }

    fn lyrics_session(&self) -> Option<Value> {
        self.core.lyrics_session.lock().unwrap().clone()
    }

    fn media_session(&self) -> Option<Value> {
        self.core.media_session.lock().unwrap().clone()
    }

    fn set_lyrics_session(&self, session: Value) {
        *self.core.lyrics_session.lock().unwrap() = Some(session);
        *self.core.media_session.lock().unwrap() = None;
        *self.core.active_entry_kind.lock().unwrap() = Some("lyrics".to_string());
        *self.core.active_session_id.lock().unwrap() = None;
        *self.core.active_audio_path.lock().unwrap() = None;
        *self.core.active_cover_path.lock().unwrap() = None;
        self.emit("stage-session-updated", self.build_status());
    }

    fn set_media_session(&self, result: MediaSessionResult) {
        *self.core.lyrics_session.lock().unwrap() = None;
        *self.core.media_session.lock().unwrap() = Some(result.session.clone());
        *self.core.active_entry_kind.lock().unwrap() = Some("media".to_string());
        *self.core.active_session_id.lock().unwrap() = Some(result.session_id.clone());
        *self.core.active_audio_path.lock().unwrap() = result.audio_path.clone();
        *self.core.active_cover_path.lock().unwrap() = result.cover_path.clone();
        self.remember_session_assets(&result.session_id, &result);
        self.cleanup_inactive_sessions();
        self.emit("stage-session-updated", self.build_status());
    }

    fn remember_session_assets(&self, session_id: &str, result: &MediaSessionResult) {
        let mut index = self.core.session_assets.lock().unwrap();
        index.retain(|(id, _)| id != session_id);
        index.push_back((
            session_id.to_string(),
            SessionAssets {
                cover_path: result.cover_path.clone(),
                cover_mime_type: result.cover_mime_type.clone(),
                working_directory: result.working_directory.clone(),
            },
        ));
    }

    fn cleanup_inactive_sessions(&self) {
        let to_remove: Vec<PathBuf> = {
            let mut index = self.core.session_assets.lock().unwrap();
            let retained: Vec<String> = index
                .iter()
                .skip(index.len().saturating_sub(STAGE_SESSION_RETENTION_LIMIT))
                .map(|(id, _)| id.clone())
                .collect();
            let active = self.core.active_session_id.lock().unwrap().clone();
            let keep: Vec<String> = match active {
                Some(active) if !retained.contains(&active) => {
                    let mut v = retained;
                    v.push(active);
                    v
                }
                _ => retained,
            };
            let to_remove: Vec<PathBuf> = index
                .iter()
                .filter(|(id, _)| !keep.contains(id))
                .map(|(_, assets)| assets.working_directory.clone())
                .collect();
            index.retain(|(id, _)| keep.contains(id));
            drop(index);
            to_remove
        };
        for dir in to_remove {
            std::thread::spawn(move || {
                let _ = std::fs::remove_dir_all(dir);
            });
        }
    }

    fn session_assets(&self, session_id: &str) -> Option<SessionAssets> {
        self.core
            .session_assets
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, assets)| assets.clone())
    }

    fn current_media_audio_path(&self) -> Option<PathBuf> {
        let kind = self.core.active_entry_kind.lock().unwrap().clone();
        if kind.as_deref() != Some("media") {
            return None;
        }
        let media = self.core.media_session.lock().unwrap().clone();
        let audio_src = media
            .as_ref()
            .and_then(|m| m.get("audioSrc").and_then(Value::as_str))
            .unwrap_or("");
        if !audio_src.starts_with("http://127.0.0.1:") {
            return None;
        }
        self.core.active_audio_path.lock().unwrap().clone()
    }

    fn current_cover_path(&self) -> Option<PathBuf> {
        self.core.active_cover_path.lock().unwrap().clone()
    }

    fn current_cover_mime_type(&self) -> String {
        self.core
            .media_session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|m| m.get("coverMimeType").and_then(Value::as_str))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string())
    }

    fn current_audio_mime_type(&self) -> String {
        self.core
            .media_session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|m| m.get("audioMimeType").and_then(Value::as_str))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string())
    }

    fn clear_state_data(&self) {
        self.core.play_pending.cancel_all(StageError::new(
            503,
            "STAGE_PLAY_CANCELED",
            "Stage state was cleared.",
        ));
        self.clear_session_state();
    }

    /// IPC / HTTP DELETE `/stage/state` entry point (mirrors clearStageState).
    pub fn clear_state(&self) -> Value {
        self.clear_state_data();
        let status = self.build_status();
        self.emit("stage-session-cleared", status.clone());
        self.emit("stage-session-updated", status.clone());
        status
    }

    pub fn regenerate_token(&self, settings: &SettingsStore) -> Result<Value, String> {
        let token = generate_token();
        settings.set(STAGE_API_TOKEN_KEY.to_string(), json!(token.clone()))?;
        *self.core.token.write().unwrap() = Some(token);
        let _ = self.core.ws_kick.send(());
        let _ = self.start_server();
        let status = self.build_status();
        self.emit("stage-session-updated", status.clone());
        Ok(status)
    }

    // -- player snapshot ------------------------------------------------------

    fn current_player_snapshot(&self) -> Option<Value> {
        self.core.player_snapshot.lock().unwrap().clone()
    }

    fn current_player_snapshot_or_fallback(&self) -> Value {
        self.current_player_snapshot()
            .unwrap_or_else(fallback_player_snapshot)
    }

    /// Mirror `publishStagePlayerSnapshot`: normalize, compare track/queue/playback
    /// keys, and broadcast the matching player event over the Stage WebSocket.
    pub fn publish_player_snapshot(&self, snapshot: Value, options: Option<Value>) -> Value {
        let previous = self.current_player_snapshot();
        let next = normalize_player_snapshot(&snapshot);
        *self.core.player_snapshot.lock().unwrap() = Some(next.clone());
        let next_track_key = track_key(&next);
        let next_queue_key = queue_key(Some(&next));
        let next_playback_key = playback_key(&next);
        let force_playback_event = options
            .as_ref()
            .and_then(|o| o.get("forcePlaybackEvent"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let status = build_player_status_from(&next);
        let mut prev_track = self.core.track_key.lock().unwrap();
        let mut prev_queue = self.core.queue_key.lock().unwrap();
        let mut prev_playback = self.core.playback_key.lock().unwrap();

        let event = if prev_track.is_some()
            && prev_track.as_deref() != Some(next_track_key.as_str())
        {
            Some(("TRACK_CHANGED", build_track_event(&next)))
        } else if prev_queue.is_some() && prev_queue.as_deref() != Some(next_queue_key.as_str()) {
            Some(("QUEUE_UPDATED", build_queue_event(&next, previous.as_ref())))
        } else if force_playback_event
            || (prev_playback.is_some()
                && prev_playback.as_deref() != Some(next_playback_key.as_str()))
        {
            Some(("PLAYBACK_UPDATED", build_time_from(&next)))
        } else {
            None
        };

        *prev_track = Some(next_track_key);
        *prev_queue = Some(next_queue_key);
        *prev_playback = Some(next_playback_key);
        drop(prev_track);
        drop(prev_queue);
        drop(prev_playback);

        if let Some((event_name, payload)) = event {
            let message = ws_message(event_name, &payload);
            let _ = self.core.player_events.send(message);
        }
        status
    }

    pub fn build_player_status(&self) -> Value {
        let snapshot = self.current_player_snapshot_or_fallback();
        build_player_status_from(&snapshot)
    }

    fn build_player_time(&self) -> Value {
        let snapshot = self.current_player_snapshot_or_fallback();
        build_time_from(&snapshot)
    }

    fn build_player_queue_window(&self, query: Option<&str>) -> Value {
        let snapshot = self.current_player_snapshot_or_fallback();
        let (offset, limit) = normalize_queue_window(query, &snapshot);
        let queue = snapshot.get("queue").cloned().unwrap_or_else(|| json!({}));
        let items = queue
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let length = queue
            .get("length")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| items.len() as i64)
            .max(0);
        let sliced: Vec<Value> = items
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        let next_offset = if (offset + sliced.len() as i64) < length {
            Some(offset + sliced.len() as i64)
        } else {
            None
        };
        let status = build_player_status_from(&snapshot);
        let mut summary = queue_summary(&snapshot);
        let obj = summary.as_object_mut().unwrap();
        obj.insert("items".to_string(), json!(sliced));
        obj.insert("offset".to_string(), json!(offset));
        obj.insert("limit".to_string(), json!(limit));
        obj.insert("returned".to_string(), json!(sliced.len() as i64));
        obj.insert("hasMore".to_string(), json!(next_offset.is_some()));
        obj.insert("nextOffset".to_string(), json!(next_offset));
        json!({
            "domain": "player-playback",
            "direction": "inside-out",
            "playbackContext": status["playbackContext"],
            "queueCapabilities": status["queueCapabilities"],
            "queue": summary,
        })
    }

    // -- renderer request bridge ----------------------------------------------

    async fn request_song_play(
        &self,
        song_id: i64,
        append_to_queue: bool,
    ) -> Result<Value, StageError> {
        let request_id = format!("stage-play-{}-{}", now_ms(), random_suffix());
        let timeout_error = StageError::new(
            504,
            "STAGE_PLAY_TIMEOUT",
            "Stage external play request timed out.",
        )
        .with_details(json!({ "requestId": request_id, "songId": song_id }));
        let rx = self.core.play_pending.register(
            &request_id,
            STAGE_PLAY_REQUEST_TIMEOUT_MS,
            timeout_error,
        );
        (self.core.emit)(
            "stage-external-play-request",
            json!({
                "requestId": request_id,
                "songId": song_id,
                "appendToQueue": append_to_queue,
            }),
        );
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(StageError::new(
                503,
                "STAGE_PLAY_UNAVAILABLE",
                "Folia main window is unavailable for external play requests.",
            )),
        }
    }

    async fn request_player_control(&self, payload: &Value) -> Result<Value, StageError> {
        let request_id = format!("stage-player-control-{}-{}", now_ms(), random_suffix());
        let timeout_error = StageError::new(
            504,
            "STAGE_PLAYER_REQUEST_TIMEOUT",
            "Stage player request timed out.",
        )
        .with_details(json!({ "requestId": request_id }));
        let rx = self.core.control_pending.register(
            &request_id,
            STAGE_PLAYER_REQUEST_TIMEOUT_MS,
            timeout_error,
        );
        let mut event_payload = json!({ "requestId": request_id });
        if let Some(obj) = payload.as_object() {
            for (k, v) in obj {
                event_payload[k] = v.clone();
            }
        }
        (self.core.emit)("stage-player-control-request", event_payload);
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(StageError::new(
                503,
                "STAGE_PLAYER_CONTROL_UNAVAILABLE",
                "Folia main window is unavailable for Stage player requests.",
            )),
        }
    }

    async fn request_player_queue(&self, payload: &Value) -> Result<Value, StageError> {
        let request_id = format!("stage-player-queue-{}-{}", now_ms(), random_suffix());
        let timeout_error = StageError::new(
            504,
            "STAGE_PLAYER_REQUEST_TIMEOUT",
            "Stage player request timed out.",
        )
        .with_details(json!({ "requestId": request_id }));
        let rx = self.core.queue_pending.register(
            &request_id,
            STAGE_PLAYER_REQUEST_TIMEOUT_MS,
            timeout_error,
        );
        let mut event_payload = json!({ "requestId": request_id });
        if let Some(obj) = payload.as_object() {
            for (k, v) in obj {
                event_payload[k] = v.clone();
            }
        }
        (self.core.emit)("stage-player-queue-request", event_payload);
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(StageError::new(
                503,
                "STAGE_PLAYER_QUEUE_UNAVAILABLE",
                "Folia main window is unavailable for Stage player requests.",
            )),
        }
    }

    /// Mirror `completeStageExternalPlayRequest`.
    pub fn complete_external_play(&self, result: &Value) -> bool {
        let request_id = normalize_stage_text(result.get("requestId").unwrap_or(&Value::Null));
        if request_id.is_empty() {
            return false;
        }
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let previous = self.current_player_snapshot_or_fallback();
        if let Some(snapshot) = result.get("snapshot") {
            if !snapshot.is_null() {
                self.publish_player_snapshot(snapshot.clone(), None);
            }
        }
        let next = self.current_player_snapshot_or_fallback();
        if ok {
            let base = result
                .get("baseSnapshot")
                .filter(|v| !v.is_null())
                .unwrap_or(&previous);
            let result_value = result
                .get("result")
                .cloned()
                .unwrap_or_else(|| json!({ "ok": true }));
            let resolved = with_queue_diff_revisions(result_value, Some(base), &next);
            self.core.play_pending.complete(&request_id, Ok(resolved))
        } else {
            let error = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Renderer rejected the Stage play request.");
            let err = StageError::new(502, "STAGE_PLAY_REJECTED", error)
                .with_details(json!({ "requestId": request_id }));
            self.core.play_pending.complete(&request_id, Err(err))
        }
    }

    /// Mirror `completeStagePlayerRendererRequest` for control/queue requests.
    fn complete_player_request(
        &self,
        request_id: &str,
        result: &Value,
        pending: &PendingStore,
    ) -> bool {
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let previous = self.current_player_snapshot_or_fallback();
        if let Some(snapshot) = result.get("snapshot") {
            if !snapshot.is_null() {
                self.publish_player_snapshot(snapshot.clone(), None);
            }
        }
        let next = self.current_player_snapshot_or_fallback();
        if ok {
            let result_value = result
                .get("result")
                .cloned()
                .unwrap_or_else(|| json!({ "ok": true }));
            let resolved = with_queue_diff_revisions(result_value, Some(&previous), &next);
            pending.complete(request_id, Ok(resolved))
        } else {
            let error = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Renderer rejected the Stage player request.");
            let err = StageError::new(502, "STAGE_PLAYER_REQUEST_REJECTED", error)
                .with_details(json!({ "requestId": request_id }));
            pending.complete(request_id, Err(err))
        }
    }

    pub fn complete_player_control(&self, result: &Value) -> bool {
        let request_id = normalize_stage_text(result.get("requestId").unwrap_or(&Value::Null));
        if request_id.is_empty() {
            return false;
        }
        self.complete_player_request(&request_id, result, &self.core.control_pending)
    }

    pub fn complete_player_queue(&self, result: &Value) -> bool {
        let request_id = normalize_stage_text(result.get("requestId").unwrap_or(&Value::Null));
        if request_id.is_empty() {
            return false;
        }
        self.complete_player_request(&request_id, result, &self.core.queue_pending)
    }

    // -- search ----------------------------------------------------------------

    async fn search_songs(&self, query: &str, limit: i64) -> Result<Value, StageError> {
        let port = (self.core.netease_port)();
        if port == 0 {
            return Err(StageError::new(
                503,
                "NETEASE_API_UNAVAILABLE",
                "Local Netease API is unavailable.",
            ));
        }
        let endpoint = format!(
            "http://127.0.0.1:{port}/cloudsearch?keywords={}&limit={}&offset=0",
            percent_encode(query),
            limit
        );
        let response = self.core.client.get(&endpoint).send().await.map_err(|e| {
            StageError::new(
                502,
                "NETEASE_SEARCH_FAILED",
                format!("Failed to search songs through the local Netease API: {e}"),
            )
        })?;
        if !response.status().is_success() {
            return Err(StageError::new(
                502,
                "NETEASE_SEARCH_FAILED",
                "Failed to search songs through the local Netease API.",
            ));
        }
        let payload: Value = response.json().await.map_err(|e| {
            StageError::new(
                502,
                "NETEASE_SEARCH_FAILED",
                format!("Failed to parse Netease search payload: {e}"),
            )
        })?;
        let songs = payload
            .get("result")
            .and_then(|r| r.get("songs"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let normalized: Vec<Value> = songs.iter().filter_map(normalize_search_result).collect();
        Ok(json!(normalized))
    }

    // -- sessions ---------------------------------------------------------------

    async fn create_media_session_from_json(
        &self,
        payload: &Value,
        working_directory: &Path,
    ) -> Result<MediaSessionResult, StageError> {
        let fields = json_payload_fields(payload);
        let session_id = if fields.session_id.trim().is_empty() {
            format!("stage-{}-{}", now_ms(), random_uuid())
        } else {
            fields.session_id.trim().to_string()
        };
        let requested_lyrics_format = fields.lyrics_format.clone();
        if !requested_lyrics_format.is_empty() && !is_stage_lyrics_format(&requested_lyrics_format)
        {
            return Err(StageError::validation(
                "Invalid lyricsFormat. Only \"lrc\", \"enhanced-lrc\", \"vtt\", and \"yrc\" are supported.",
                "INVALID_LYRICS_FORMAT",
            )
            .with_details(json!({ "lyricsFormat": requested_lyrics_format })));
        }
        let audio_url = fields.audio_url.clone();
        if audio_url.is_empty() {
            return Err(StageError::validation(
                "Provide exactly one audio source: either audioUrl or audioFile.",
                "INVALID_AUDIO_SOURCE",
            )
            .with_details(json!({ "hasAudioUrl": false, "hasAudioFile": false })));
        }
        let session_version = now_ms();
        let cover_url = fields.cover_url.clone();
        let cover = if cover_url.is_empty() {
            Value::Null
        } else {
            json!(cover_url)
        };
        let lyrics_text = fields.lyrics_text.clone();
        let lyrics_value = if lyrics_text.is_empty() {
            Value::Null
        } else {
            json!(lyrics_text.clone())
        };
        let detected_format = if !lyrics_text.is_empty() {
            if requested_lyrics_format.is_empty() {
                detect_stage_lyrics_format(&lyrics_text)
            } else {
                Some(requested_lyrics_format.clone())
            }
        } else {
            None
        };
        let title = fields.title.clone();
        let artist = fields.artist.clone();
        let session = json!({
            "id": session_id,
            "title": if title.is_empty() { "Stage Session".to_string() } else { title },
            "artist": if artist.is_empty() { "Stage".to_string() } else { artist },
            "album": fields.album,
            "durationMs": Value::Null,
            "coverUrl": cover.clone(),
            "coverArtUrl": cover,
            "audioUrl": json!(audio_url.clone()),
            "audioSrc": json!(audio_url),
            "lyricsText": lyrics_value,
            "lyricsFormat": json!(detected_format),
            "updatedAt": session_version,
        });
        Ok(MediaSessionResult {
            session,
            session_id,
            audio_path: None,
            cover_path: None,
            cover_mime_type: None,
            working_directory: working_directory.to_path_buf(),
        })
    }

    async fn create_media_session_from_multipart(
        &self,
        parsed: MultipartPayload,
        working_directory: &Path,
    ) -> Result<MediaSessionResult, StageError> {
        let fields = parsed.fields;
        let get_field = |key: &str| -> String { fields.get(key).cloned().unwrap_or_default() };
        let session_id = {
            let sid = get_field("sessionId");
            if sid.trim().is_empty() {
                format!("stage-{}-{}", now_ms(), random_uuid())
            } else {
                sid.trim().to_string()
            }
        };
        let requested_title = get_field("title").trim().to_string();
        let requested_artist = get_field("artist").trim().to_string();
        let requested_album = get_field("album").trim().to_string();
        let requested_cover_url = get_field("coverUrl").trim().to_string();
        let requested_audio_url = get_field("audioUrl").trim().to_string();
        let requested_lyrics_text = get_field("lyricsText").trim().to_string();
        let requested_lyrics_format = get_field("lyricsFormat").trim().to_string();

        let audio_file = parsed.files.get("audioFile");
        let lyrics_file = parsed.files.get("lyricsFile");
        let cover_file = parsed.files.get("coverFile");

        if !requested_lyrics_format.is_empty() && !is_stage_lyrics_format(&requested_lyrics_format)
        {
            return Err(StageError::validation(
                "Invalid lyricsFormat. Only \"lrc\", \"enhanced-lrc\", \"vtt\", and \"yrc\" are supported.",
                "INVALID_LYRICS_FORMAT",
            )
            .with_details(json!({ "lyricsFormat": requested_lyrics_format })));
        }
        let has_audio_url = !requested_audio_url.is_empty();
        let has_audio_file = audio_file.is_some();
        if has_audio_url == has_audio_file {
            return Err(StageError::validation(
                "Provide exactly one audio source: either audioUrl or audioFile.",
                "INVALID_AUDIO_SOURCE",
            )
            .with_details(
                json!({ "hasAudioUrl": has_audio_url, "hasAudioFile": has_audio_file }),
            ));
        }
        let has_lyrics_text = !requested_lyrics_text.is_empty();
        let has_lyrics_file = lyrics_file.is_some();
        if has_lyrics_text && has_lyrics_file {
            return Err(StageError::validation(
                "Provide at most one standalone lyrics source: either lyricsText or lyricsFile.",
                "INVALID_LYRICS_SOURCE",
            )
            .with_details(
                json!({ "hasLyricsText": has_lyrics_text, "hasLyricsFile": has_lyrics_file }),
            ));
        }

        let session_version = now_ms();
        let mut audio_src = requested_audio_url.clone();
        let mut cover: Option<String> = if requested_cover_url.is_empty() {
            None
        } else {
            Some(requested_cover_url)
        };
        let mut lyrics_text = if requested_lyrics_text.is_empty() {
            None
        } else {
            Some(requested_lyrics_text)
        };
        let mut audio_path = None;
        let mut cover_path = None;
        let cover_mime_type = cover_file.and_then(|f| f.content_type.clone());

        if let Some(audio_file) = audio_file {
            audio_src = build_stage_media_url(self.port(), "audio", Some(session_version));
            audio_path = Some(audio_file.file_path.clone());
        }
        if let Some(lyrics_file) = lyrics_file {
            let text = tokio::fs::read_to_string(&lyrics_file.file_path)
                .await
                .map_err(|e| {
                    StageError::new(
                        500,
                        "STAGE_INTERNAL_ERROR",
                        format!("failed to read lyrics file: {e}"),
                    )
                })?;
            lyrics_text = Some(text.trim().to_string());
        }
        if let Some(cover_file) = cover_file {
            cover = Some(build_stage_session_media_url(
                self.port(),
                &session_id,
                "cover",
                Some(session_version),
            ));
            cover_path = Some(cover_file.file_path.clone());
        }

        let lyrics_text = lyrics_text.filter(|t| !t.trim().is_empty());
        let detected_format = match &lyrics_text {
            Some(text) => {
                if !requested_lyrics_format.is_empty() {
                    Some(requested_lyrics_format.clone())
                } else {
                    detect_stage_lyrics_format(text)
                }
            }
            None => None,
        };

        let mut session = json!({
            "id": session_id,
            "title": if requested_title.is_empty() { "Stage Session".to_string() } else { requested_title },
            "artist": if requested_artist.is_empty() { "Stage".to_string() } else { requested_artist },
            "album": requested_album,
            "durationMs": Value::Null,
            "audioUrl": if has_audio_url { json!(requested_audio_url) } else { Value::Null },
            "audioSrc": json!(audio_src),
            "lyricsText": json!(lyrics_text),
            "lyricsFormat": json!(detected_format),
            "updatedAt": session_version,
        });
        match cover {
            Some(c) => {
                session["coverUrl"] = json!(c.clone());
                session["coverArtUrl"] = json!(c);
            }
            None => {
                session["coverUrl"] = Value::Null;
                session["coverArtUrl"] = Value::Null;
            }
        }
        if audio_path.is_some() {
            if let Some(mime) = audio_file.and_then(|f| f.content_type.clone()) {
                session["audioMimeType"] = json!(mime);
            }
        }
        if let Some(mime) = cover_mime_type.clone() {
            session["coverMimeType"] = json!(mime);
        }

        Ok(MediaSessionResult {
            session,
            session_id,
            audio_path,
            cover_path,
            cover_mime_type,
            working_directory: working_directory.to_path_buf(),
        })
    }
}

struct MediaSessionResult {
    session: Value,
    session_id: String,
    audio_path: Option<PathBuf>,
    cover_path: Option<PathBuf>,
    cover_mime_type: Option<String>,
    working_directory: PathBuf,
}

struct JsonPayloadFields {
    title: String,
    artist: String,
    album: String,
    cover_url: String,
    audio_url: String,
    lyrics_text: String,
    lyrics_format: String,
    session_id: String,
}

fn json_payload_fields(payload: &Value) -> JsonPayloadFields {
    JsonPayloadFields {
        title: normalize_stage_text(payload.get("title").unwrap_or(&Value::Null)),
        artist: normalize_stage_text(payload.get("artist").unwrap_or(&Value::Null)),
        album: normalize_stage_text(payload.get("album").unwrap_or(&Value::Null)),
        cover_url: normalize_stage_text(payload.get("coverUrl").unwrap_or(&Value::Null)),
        audio_url: normalize_stage_text(payload.get("audioUrl").unwrap_or(&Value::Null)),
        lyrics_text: normalize_stage_text(payload.get("lyricsText").unwrap_or(&Value::Null)),
        lyrics_format: normalize_stage_text(payload.get("lyricsFormat").unwrap_or(&Value::Null)),
        session_id: normalize_stage_text(payload.get("sessionId").unwrap_or(&Value::Null)),
    }
}

struct MultipartPayload {
    fields: HashMap<String, String>,
    files: HashMap<String, UploadedFile>,
}

struct UploadedFile {
    content_type: Option<String>,
    file_path: PathBuf,
}

fn build_stage_media_url(port: u16, kind: &str, version: Option<i64>) -> String {
    let base = format!("http://127.0.0.1:{port}/stage/media/current/{kind}");
    match version {
        Some(v) => format!("{base}?v={}", percent_encode(&v.to_string())),
        None => base,
    }
}

fn build_stage_session_media_url(
    port: u16,
    session_id: &str,
    kind: &str,
    version: Option<i64>,
) -> String {
    let base = format!(
        "http://127.0.0.1:{port}/stage/media/session/{}/{kind}",
        percent_encode(session_id)
    );
    match version {
        Some(v) => format!("{base}?v={}", percent_encode(&v.to_string())),
        None => base,
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
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

// ---------------------------------------------------------------------------
// Player snapshot normalization (mirror stageApi.cjs)
// ---------------------------------------------------------------------------

fn normalize_player_context(value: &Value) -> String {
    let s = value.as_str().unwrap_or("");
    if STAGE_PLAYER_CONTEXT_VALUES.contains(&s) {
        s.to_string()
    } else {
        "normal-playback".to_string()
    }
}

fn normalize_player_state(value: &Value) -> String {
    let s = value.as_str().unwrap_or("");
    if STAGE_PLAYER_STATE_VALUES.contains(&s) {
        s.to_string()
    } else {
        "IDLE".to_string()
    }
}

fn normalize_player_current(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    let id = normalize_stage_text(value.get("id").unwrap_or(&Value::Null));
    let title = normalize_stage_text(value.get("title").unwrap_or(&Value::Null));
    if id.is_empty() && title.is_empty() {
        return None;
    }
    let source = normalize_stage_text(value.get("source").unwrap_or(&Value::Null));
    let artist = normalize_stage_text(value.get("artist").unwrap_or(&Value::Null));
    let album = normalize_stage_text(value.get("album").unwrap_or(&Value::Null));
    let cover_url = normalize_stage_text(value.get("coverUrl").unwrap_or(&Value::Null));
    Some(json!({
        "id": if id.is_empty() { title.clone() } else { id.clone() },
        "source": if source.is_empty() { "unknown".to_string() } else { source },
        "title": if title.is_empty() { "Unknown Song".to_string() } else { title.clone() },
        "artist": artist,
        "album": album,
        "durationMs": normalize_stage_integer(value.get("durationMs").unwrap_or(&Value::Null), 0).max(0),
        "coverUrl": if cover_url.is_empty() { Value::Null } else { json!(cover_url) },
    }))
}

fn normalize_player_queue_item(item: &Value, index: i64) -> Option<Value> {
    if !item.is_object() {
        return None;
    }
    let inner = if item.get("current").is_some() {
        item.get("current")
    } else {
        Some(item)
    };
    let current = normalize_player_current(inner)?;
    let queue_item_id = normalize_stage_text(item.get("queueItemId").unwrap_or(&Value::Null));
    let fallback_id = format!(
        "{}:{}:{index}",
        current["source"].as_str().unwrap_or("unknown"),
        current["id"].as_str().unwrap_or("")
    );
    let mut out = json!({
        "queueItemId": if queue_item_id.is_empty() { fallback_id } else { queue_item_id },
    });
    if let Some(obj) = current.as_object() {
        for (k, v) in obj {
            out[k] = v.clone();
        }
    }
    Some(out)
}

fn normalize_player_queue(queue: Option<&Value>) -> Value {
    let items = queue
        .and_then(|q| q.get("items"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, item)| normalize_player_queue_item(item, i as i64))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let current_index = queue
        .and_then(|q| q.get("currentIndex"))
        .map(|v| normalize_stage_integer(v, -1))
        .unwrap_or(-1);
    let current_index = if current_index >= 0 && (current_index as usize) < items.len() {
        current_index
    } else {
        -1
    };
    json!({
        "items": items,
        "currentIndex": current_index,
        "length": items.len(),
    })
}

fn build_control_capabilities(
    context: &str,
    current: Option<&Value>,
    provided: Option<&Value>,
) -> Value {
    let has_current = current.is_some();
    let can_control_transport = context != "external-playback-source" && has_current;
    let is_normal_playback = context == "normal-playback";
    let provided = provided.unwrap_or(&Value::Null);
    let previous =
        is_normal_playback && js_truthy(provided.get("previous").unwrap_or(&Value::Null));
    let next = is_normal_playback && js_truthy(provided.get("next").unwrap_or(&Value::Null));
    json!({
        "play": js_nullish_coalesce(provided.get("play"), can_control_transport),
        "pause": js_nullish_coalesce(provided.get("pause"), can_control_transport),
        "resume": js_nullish_coalesce(provided.get("resume"), can_control_transport),
        "seek": js_nullish_coalesce(provided.get("seek"), can_control_transport),
        "previous": previous,
        "next": next,
    })
}

fn build_queue_capabilities(context: &str, provided: Option<&Value>) -> Value {
    let can_edit_queue = context == "normal-playback";
    let provided = provided.unwrap_or(&Value::Null);
    let get = |key: &str| js_nullish_coalesce(provided.get(key), can_edit_queue);
    json!({
        "append": get("append"),
        "insertNext": get("insertNext"),
        "remove": get("remove"),
        "move": get("move"),
        "select": get("select"),
        "clear": get("clear"),
    })
}

fn normalize_player_snapshot(snapshot: &Value) -> Value {
    let source = if snapshot.is_object() {
        snapshot
    } else {
        &Value::Null
    };
    let playback_context =
        normalize_player_context(source.get("playbackContext").unwrap_or(&Value::Null));
    let current = normalize_player_current(source.get("current"));
    let queue = normalize_player_queue(source.get("queue"));
    let raw_duration = source.get("durationMs").cloned();
    let duration_value = if raw_duration.is_some() {
        raw_duration
    } else {
        current
            .as_ref()
            .map(|c| c.get("durationMs").cloned())
            .flatten()
    };
    let duration_ms =
        normalize_stage_integer(duration_value.as_ref().unwrap_or(&Value::Null), 0).max(0);
    let position_ms =
        normalize_stage_integer(source.get("positionMs").unwrap_or(&Value::Null), 0).max(0);
    let sampled_at_ms =
        normalize_stage_integer(source.get("sampledAtMs").unwrap_or(&Value::Null), now_ms()).max(0);
    let updated_at =
        normalize_stage_integer(source.get("updatedAt").unwrap_or(&Value::Null), now_ms()).max(0);
    let position_ms = if duration_ms > 0 {
        position_ms.min(duration_ms)
    } else {
        position_ms
    };
    json!({
        "playbackContext": playback_context.clone(),
        "current": current,
        "playerState": normalize_player_state(source.get("playerState").unwrap_or(&Value::Null)),
        "positionMs": position_ms,
        "durationMs": duration_ms,
        "sampledAtMs": sampled_at_ms,
        "updatedAt": updated_at,
        "controlCapabilities": build_control_capabilities(
            &playback_context,
            current.as_ref(),
            source.get("controlCapabilities"),
        ),
        "queueCapabilities": build_queue_capabilities(&playback_context, source.get("queueCapabilities")),
        "queue": queue,
    })
}

fn fallback_player_snapshot() -> Value {
    json!({
        "playbackContext": "normal-playback",
        "current": Value::Null,
        "playerState": "IDLE",
        "positionMs": 0,
        "durationMs": 0,
        "sampledAtMs": now_ms(),
        "updatedAt": now_ms(),
        "controlCapabilities": {
            "play": false, "pause": false, "resume": false,
            "seek": false, "previous": false, "next": false,
        },
        "queueCapabilities": {
            "append": true, "insertNext": true, "remove": true,
            "move": true, "select": true, "clear": true,
        },
        "queue": { "items": [], "currentIndex": -1, "length": 0 },
    })
}

fn resolve_snapshot_time(snapshot: &Value) -> (i64, i64, i64) {
    let now = now_ms();
    let mut position_ms = snapshot
        .get("positionMs")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if snapshot.get("playerState").and_then(Value::as_str) == Some("PLAYING") {
        if let Some(sampled) = snapshot.get("sampledAtMs").and_then(Value::as_i64) {
            position_ms += (now - sampled).max(0);
        }
    }
    let duration_ms = snapshot
        .get("durationMs")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    if duration_ms > 0 {
        position_ms = position_ms.min(duration_ms);
    }
    (position_ms.max(0), duration_ms, now)
}

fn queue_summary(snapshot: &Value) -> Value {
    let queue = snapshot.get("queue").unwrap_or(&Value::Null);
    let current_index = queue
        .get("currentIndex")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let length = queue
        .get("length")
        .and_then(Value::as_i64)
        .or_else(|| {
            queue
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.len() as i64)
        })
        .unwrap_or(0)
        .max(0);
    json!({
        "currentIndex": current_index,
        "length": length,
        "revision": queue_revision(Some(snapshot)),
    })
}

fn build_player_status_from(snapshot: &Value) -> Value {
    let (position_ms, duration_ms, sampled_at_ms) = resolve_snapshot_time(snapshot);
    json!({
        "domain": "player-playback",
        "direction": "inside-out",
        "playbackContext": snapshot.get("playbackContext"),
        "current": snapshot.get("current"),
        "playerState": snapshot.get("playerState"),
        "positionMs": position_ms,
        "durationMs": duration_ms,
        "sampledAtMs": sampled_at_ms,
        "updatedAt": snapshot.get("updatedAt"),
        "controlCapabilities": snapshot.get("controlCapabilities"),
        "queueCapabilities": snapshot.get("queueCapabilities"),
        "queue": queue_summary(snapshot),
    })
}

fn build_time_from(snapshot: &Value) -> Value {
    let (position_ms, duration_ms, sampled_at_ms) = resolve_snapshot_time(snapshot);
    json!({
        "domain": "player-playback",
        "direction": "inside-out",
        "playbackContext": snapshot.get("playbackContext"),
        "playerState": snapshot.get("playerState"),
        "positionMs": position_ms,
        "durationMs": duration_ms,
        "sampledAtMs": sampled_at_ms,
    })
}

fn build_track_event(snapshot: &Value) -> Value {
    let status = build_player_status_from(snapshot);
    json!({
        "domain": "player-playback",
        "direction": "inside-out",
        "playbackContext": status.get("playbackContext"),
        "current": status.get("current"),
        "playerState": status.get("playerState"),
        "sampledAtMs": status.get("sampledAtMs"),
        "updatedAt": status.get("updatedAt"),
        "controlCapabilities": status.get("controlCapabilities"),
        "queueCapabilities": status.get("queueCapabilities"),
        "queue": status.get("queue"),
    })
}

fn build_queue_event(snapshot: &Value, previous: Option<&Value>) -> Value {
    let status = build_player_status_from(snapshot);
    let mut payload = json!({
        "domain": "player-playback",
        "direction": "inside-out",
        "playbackContext": status.get("playbackContext"),
        "current": status.get("current"),
        "queueCapabilities": status.get("queueCapabilities"),
        "queue": status.get("queue"),
    });
    if let Some(previous) = previous {
        payload["previousQueue"] = queue_summary(previous);
    }
    payload
}

fn ws_message(event_name: &str, payload: &Value) -> String {
    let mut msg = json!({ "event": event_name });
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj {
            msg[k] = v.clone();
        }
    }
    serde_json::to_string(&msg).unwrap_or_default()
}

fn track_key(snapshot: &Value) -> String {
    match snapshot.get("current") {
        Some(current) if current.is_object() => format!(
            "{}:{}",
            normalize_stage_text(current.get("source").unwrap_or(&Value::Null)),
            normalize_stage_text(current.get("id").unwrap_or(&Value::Null))
        ),
        _ => "none".to_string(),
    }
}

fn queue_key(snapshot: Option<&Value>) -> String {
    let Some(snapshot) = snapshot else {
        return String::new();
    };
    let Some(items) = snapshot
        .get("queue")
        .and_then(|q| q.get("items"))
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|i| i.get("queueItemId").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("|")
}

fn queue_revision(snapshot: Option<&Value>) -> String {
    let key = queue_key(snapshot);
    format!("{:x}", Sha1::digest(key.as_bytes()))
}

fn playback_key(snapshot: &Value) -> String {
    let position = if snapshot.get("playerState").and_then(Value::as_str) == Some("PAUSED") {
        snapshot.get("positionMs").cloned()
    } else {
        None
    };
    format!(
        "{}|{}|{:?}",
        snapshot
            .get("playbackContext")
            .and_then(Value::as_str)
            .unwrap_or(""),
        snapshot
            .get("playerState")
            .and_then(Value::as_str)
            .unwrap_or(""),
        position
    )
}

fn with_queue_diff_revisions(mut result: Value, previous: Option<&Value>, next: &Value) -> Value {
    if let Some(diff) = result.get_mut("diff") {
        if diff.is_object() {
            let obj = diff.as_object_mut().unwrap();
            obj.insert("baseRevision".to_string(), json!(queue_revision(previous)));
            obj.insert("revision".to_string(), json!(queue_revision(Some(next))));
        }
    }
    result
}

fn normalize_queue_window(query: Option<&str>, snapshot: &Value) -> (i64, i64) {
    let queue = snapshot.get("queue").unwrap_or(&Value::Null);
    let length = queue
        .get("length")
        .and_then(Value::as_i64)
        .or_else(|| {
            queue
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.len() as i64)
        })
        .unwrap_or(0)
        .max(0);
    let params = parse_query(query);
    let raw_limit = params
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(STAGE_PLAYER_QUEUE_DEFAULT_LIMIT);
    let limit = raw_limit.clamp(1, STAGE_PLAYER_QUEUE_MAX_LIMIT);
    let around = params.get("around").cloned();
    let mut offset = params
        .get("offset")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    if around.as_deref() == Some("current") {
        let current_index = queue
            .get("currentIndex")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        offset = if current_index >= 0 {
            (current_index - limit / 2).max(0)
        } else {
            0
        };
    }
    if length <= 0 {
        offset = 0;
    } else if around.as_deref() == Some("current") {
        offset = offset.min((length - limit).max(0));
    } else {
        offset = offset.min(length);
    }
    (offset, limit)
}

// ---------------------------------------------------------------------------
// Lyrics / search normalization
// ---------------------------------------------------------------------------

fn normalize_netease_lyric_branch(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    let lyric = normalize_stage_text(value.get("lyric").unwrap_or(&Value::Null));
    let pure_music = value.get("pureMusic").and_then(Value::as_bool);
    if lyric.is_empty() && pure_music.is_none() {
        return None;
    }
    let mut out = Value::Object(Default::default());
    if !lyric.is_empty() {
        out["lyric"] = json!(lyric);
    }
    if let Some(pm) = pure_music {
        out["pureMusic"] = json!(pm);
    }
    Some(out)
}

fn normalize_lyrics_session_payload(payload: &Value) -> Result<Value, StageError> {
    let raw_lyric_source = payload.get("lyricSource");
    let Some(raw_lyric_source) = raw_lyric_source else {
        return Err(StageError::validation(
            "Stage lyrics payload requires a parser-compatible lyricSource object.",
            "INVALID_STAGE_LYRICS",
        ));
    };
    if !raw_lyric_source.is_object() {
        return Err(StageError::validation(
            "Stage lyrics payload requires a parser-compatible lyricSource object.",
            "INVALID_STAGE_LYRICS",
        ));
    }
    let source_type = normalize_stage_text(raw_lyric_source.get("type").unwrap_or(&Value::Null));
    if source_type.is_empty() {
        return Err(StageError::validation(
            "Stage lyricSource.type is required.",
            "INVALID_STAGE_LYRICS",
        ));
    }

    let lyric_source = match source_type.as_str() {
        "local" => {
            let lrc_content =
                normalize_stage_text(raw_lyric_source.get("lrcContent").unwrap_or(&Value::Null));
            let t_lrc_content =
                normalize_stage_text(raw_lyric_source.get("tLrcContent").unwrap_or(&Value::Null));
            let format_hint =
                normalize_stage_text(raw_lyric_source.get("formatHint").unwrap_or(&Value::Null));
            if lrc_content.is_empty() {
                return Err(StageError::validation(
                    "Stage local lyricSource requires lrcContent.",
                    "INVALID_STAGE_LYRICS",
                ));
            }
            let mut out = json!({ "type": "local", "lrcContent": lrc_content });
            if !t_lrc_content.is_empty() {
                out["tLrcContent"] = json!(t_lrc_content);
            }
            if !format_hint.is_empty() && is_stage_lyrics_format(&format_hint) {
                out["formatHint"] = json!(format_hint);
            }
            out
        }
        "embedded" => {
            let text_content =
                normalize_stage_text(raw_lyric_source.get("textContent").unwrap_or(&Value::Null));
            let translation_content = normalize_stage_text(
                raw_lyric_source
                    .get("translationContent")
                    .unwrap_or(&Value::Null),
            );
            let uslt_tags: Vec<Value> = raw_lyric_source
                .get("usltTags")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|tag| {
                            let text =
                                normalize_stage_text(tag.get("text").unwrap_or(&Value::Null));
                            if text.is_empty() {
                                return None;
                            }
                            let mut out = json!({ "text": text });
                            let language =
                                normalize_stage_text(tag.get("language").unwrap_or(&Value::Null));
                            let descriptor =
                                normalize_stage_text(tag.get("descriptor").unwrap_or(&Value::Null));
                            if !language.is_empty() {
                                out["language"] = json!(language);
                            }
                            if !descriptor.is_empty() {
                                out["descriptor"] = json!(descriptor);
                            }
                            Some(out)
                        })
                        .collect()
                })
                .unwrap_or_default();
            if text_content.is_empty() && translation_content.is_empty() && uslt_tags.is_empty() {
                return Err(StageError::validation(
                    "Stage embedded lyricSource requires textContent, translationContent, or usltTags.",
                    "INVALID_STAGE_LYRICS",
                ));
            }
            let mut out = json!({ "type": "embedded" });
            if !text_content.is_empty() {
                out["textContent"] = json!(text_content);
            }
            if !translation_content.is_empty() {
                out["translationContent"] = json!(translation_content);
            }
            if !uslt_tags.is_empty() {
                out["usltTags"] = json!(uslt_tags);
            }
            out
        }
        "navidrome" => {
            let plain_lyrics =
                normalize_stage_text(raw_lyric_source.get("plainLyrics").unwrap_or(&Value::Null));
            let structured_lyrics: Vec<Value> = raw_lyric_source
                .get("structuredLyrics")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|line| {
                            let value =
                                normalize_stage_text(line.get("value").unwrap_or(&Value::Null));
                            let start = line.get("start").and_then(Value::as_f64);
                            if value.is_empty() && start.is_none() {
                                return None;
                            }
                            let mut out = Value::Object(Default::default());
                            if let Some(start) = start {
                                out["start"] = json!(start);
                            }
                            if !value.is_empty() {
                                out["value"] = json!(value);
                            }
                            Some(out)
                        })
                        .collect()
                })
                .unwrap_or_default();
            if plain_lyrics.is_empty() && structured_lyrics.is_empty() {
                return Err(StageError::validation(
                    "Stage navidrome lyricSource requires plainLyrics or structuredLyrics.",
                    "INVALID_STAGE_LYRICS",
                ));
            }
            let mut out = json!({ "type": "navidrome" });
            if !plain_lyrics.is_empty() {
                out["plainLyrics"] = json!(plain_lyrics);
            }
            if !structured_lyrics.is_empty() {
                out["structuredLyrics"] = json!(structured_lyrics);
            }
            out
        }
        "netease" => {
            let lrc = normalize_netease_lyric_branch(raw_lyric_source.get("lrc"));
            let yrc = normalize_netease_lyric_branch(raw_lyric_source.get("yrc"));
            let ytlrc = normalize_netease_lyric_branch(raw_lyric_source.get("ytlrc"));
            let tlyric = normalize_netease_lyric_branch(raw_lyric_source.get("tlyric"));
            let lrc_yrc = normalize_netease_lyric_branch(
                raw_lyric_source.get("lrc").and_then(|l| l.get("yrc")),
            );
            let lrc_ytlrc = normalize_netease_lyric_branch(
                raw_lyric_source.get("lrc").and_then(|l| l.get("ytlrc")),
            );
            let pure_music = raw_lyric_source.get("pureMusic").and_then(Value::as_bool);
            if lrc.is_none()
                && yrc.is_none()
                && ytlrc.is_none()
                && tlyric.is_none()
                && lrc_yrc.is_none()
                && lrc_ytlrc.is_none()
                && pure_music.is_none()
            {
                return Err(StageError::validation(
                    "Stage netease lyricSource requires at least one lyric branch.",
                    "INVALID_STAGE_LYRICS",
                ));
            }
            let mut out = json!({ "type": "netease" });
            if lrc.is_some() || lrc_yrc.is_some() || lrc_ytlrc.is_some() {
                let mut lrc_obj = lrc.unwrap_or_else(|| json!({}));
                if let Some(ly) = lrc_yrc {
                    lrc_obj["yrc"] = ly;
                }
                if let Some(lyt) = lrc_ytlrc {
                    lrc_obj["ytlrc"] = lyt;
                }
                out["lrc"] = lrc_obj;
            }
            if let Some(y) = yrc {
                out["yrc"] = y;
            }
            if let Some(y) = ytlrc {
                out["ytlrc"] = y;
            }
            if let Some(t) = tlyric {
                out["tlyric"] = t;
            }
            if let Some(pm) = pure_music {
                out["pureMusic"] = json!(pm);
            }
            out
        }
        _ => {
            return Err(StageError::validation(
                "Stage lyricSource.type must be embedded, local, navidrome, or netease.",
                "INVALID_STAGE_LYRICS",
            ))
        }
    };

    let mut session = json!({
        "lyricSource": lyric_source,
        "updatedAt": now_ms(),
    });
    for key in ["title", "artist", "album"] {
        let value = normalize_stage_text(payload.get(key).unwrap_or(&Value::Null));
        if !value.is_empty() {
            session[key] = json!(value);
        }
    }
    Ok(session)
}

fn normalize_search_result(song: &Value) -> Option<Value> {
    let artists: Vec<String> = if let Some(arr) = song.get("ar").and_then(Value::as_array) {
        arr.iter()
            .filter_map(|a| {
                normalize_stage_text(a.get("name").unwrap_or(&Value::Null)).into_option()
            })
            .collect()
    } else if let Some(arr) = song.get("artists").and_then(Value::as_array) {
        arr.iter()
            .filter_map(|a| {
                normalize_stage_text(a.get("name").unwrap_or(&Value::Null)).into_option()
            })
            .collect()
    } else {
        Vec::new()
    };
    let cover_url = song
        .get("al")
        .and_then(|al| al.get("picUrl"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or_else(|| {
            song.get("album")
                .and_then(|a| a.get("picUrl"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .or_else(|| {
            song.get("simpleSong")
                .and_then(|s| s.get("al"))
                .and_then(|al| al.get("picUrl"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .or_else(|| {
            song.get("simpleSong")
                .and_then(|s| s.get("album"))
                .and_then(|a| a.get("picUrl"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        });
    let song_id = song.get("id").and_then(Value::as_i64)?;
    if song_id <= 0 {
        return None;
    }
    let title = normalize_stage_text(song.get("name").unwrap_or(&Value::Null));
    let album = song
        .get("al")
        .and_then(|al| al.get("name"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or_else(|| {
            song.get("album")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    let duration_ms = song
        .get("dt")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map(|v| (v.floor() as i64).max(0));
    Some(json!({
        "songId": song_id,
        "title": if title.is_empty() { "Unknown Song".to_string() } else { title },
        "artists": artists,
        "album": album,
        "durationMs": duration_ms,
        "coverUrl": cover_url,
    }))
}

trait OptionStringExt {
    fn into_option(self) -> Option<String>;
}

impl OptionStringExt for String {
    fn into_option(self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            Some(self)
        }
    }
}

// ---------------------------------------------------------------------------
// Router / HTTP handlers
// ---------------------------------------------------------------------------

fn build_router(state: StageState) -> Router {
    Router::new()
        .route("/stage/player/ws", axum::routing::get(handle_stage_ws))
        .fallback(handle_stage_request)
        .layer(DefaultBodyLimit::max(STAGE_MAX_BODY_BYTES))
        .with_state(state)
}

fn matches_stage_bearer_token(
    state: &StageState,
    headers: &axum::http::header::HeaderMap,
    query: Option<&str>,
) -> bool {
    match state.token() {
        Some(expected) => match bearer_token_from(headers, query) {
            Some(request_token) => constant_time_token_eq(&request_token, &expected),
            None => false,
        },
        None => false,
    }
}

async fn handle_stage_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::header::HeaderMap,
    State(state): State<StageState>,
) -> Response<Body> {
    if !state.is_stage_enabled() {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "Stage mode is disabled." }),
        );
    }
    let request_token = bearer_token_from(&headers, None).or_else(|| params.get("token").cloned());
    let authorized = match state.token() {
        Some(expected) => request_token
            .map(|t| constant_time_token_eq(&t, &expected))
            .unwrap_or(false),
        None => false,
    };
    if !authorized {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "Unauthorized." }),
        );
    }
    ws.on_upgrade(move |socket| handle_stage_ws_socket(state, socket))
}

async fn handle_stage_ws_socket(state: StageState, mut socket: WebSocket) {
    let status = state.build_player_status();
    let first = ws_message("STATUS", &status);
    if socket.send(Message::Text(first.into())).await.is_err() {
        return;
    }
    let mut events_rx = state.core.player_events.subscribe();
    let mut kick_rx = state.core.ws_kick.subscribe();
    loop {
        tokio::select! {
            kicked = kick_rx.recv() => {
                if matches!(kicked, Ok(())) {
                    let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 1008,
                        reason: "Stage bearer token changed.".into(),
                    }))).await;
                    break;
                }
            }
            msg = events_rx.recv() => {
                match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    None => break,
                    Some(Err(_)) => break,
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

async fn handle_stage_request(
    State(state): State<StageState>,
    req: Request<Body>,
) -> Result<Response<Body>, StageError> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let pathname = uri.path().to_string();
    let query = uri.query().map(|q| q.to_string());
    let headers = req.headers().clone();

    if method == Method::OPTIONS {
        return Ok(cors_no_content());
    }

    if pathname == "/stage/health" && method == Method::GET {
        return Ok(json_response(StatusCode::OK, state.build_health()));
    }

    if !state.is_stage_enabled() {
        return Ok(json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "Stage mode is disabled." }),
        ));
    }

    if pathname == "/stage/media/current/audio" && method == Method::GET {
        let Some(path) = state.current_media_audio_path() else {
            return Ok(json_response(
                StatusCode::NOT_FOUND,
                json!({ "error": "No uploaded stage audio is available." }),
            ));
        };
        let mime = state.current_audio_mime_type();
        let range = req_range(&headers);
        return serve_stage_file(req, &path, &mime, range).await;
    }

    if let Some(session_id) = pathname.strip_prefix("/stage/media/session/") {
        if method == Method::GET {
            if let Some(kind) = session_id.strip_suffix("/cover") {
                let requested_session_id = percent_decode(kind);
                let assets = state.session_assets(&requested_session_id);
                let Some(cover_path) = assets.as_ref().and_then(|a| a.cover_path.as_ref()) else {
                    return Ok(json_response(
                        StatusCode::NOT_FOUND,
                        json!({ "error": "No uploaded stage cover is available for that session." }),
                    ));
                };
                let is_current = state
                    .media_session()
                    .and_then(|m| {
                        m.get("id")
                            .and_then(Value::as_str)
                            .map(|s| s == requested_session_id)
                    })
                    .unwrap_or(false);
                let mime = if is_current {
                    state.current_cover_mime_type()
                } else {
                    assets
                        .as_ref()
                        .and_then(|a| a.cover_mime_type.clone())
                        .unwrap_or_else(|| "application/octet-stream".to_string())
                };
                let buffer = tokio::fs::read(cover_path).await.map_err(|e| {
                    StageError::new(
                        500,
                        "STAGE_INTERNAL_ERROR",
                        format!("failed to read cover: {e}"),
                    )
                })?;
                return Ok(send_binary(&buffer, &mime));
            }
        }
    }

    if pathname == "/stage/media/current/cover" && method == Method::GET {
        let Some(cover_path) = state.current_cover_path() else {
            return Ok(json_response(
                StatusCode::NOT_FOUND,
                json!({ "error": "No uploaded stage cover is available." }),
            ));
        };
        let mime = state.current_cover_mime_type();
        let buffer = tokio::fs::read(cover_path).await.map_err(|e| {
            StageError::new(
                500,
                "STAGE_INTERNAL_ERROR",
                format!("failed to read cover: {e}"),
            )
        })?;
        return Ok(send_binary(&buffer, &mime));
    }

    if !matches_stage_bearer_token(&state, &headers, query.as_deref()) {
        return Ok(json_response(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "Unauthorized." }),
        ));
    }

    if pathname == "/stage/status" && method == Method::GET {
        return Ok(json_response(StatusCode::OK, state.build_status()));
    }

    if pathname == "/stage/player/status" && method == Method::GET {
        return Ok(json_response(StatusCode::OK, state.build_player_status()));
    }

    if pathname == "/stage/player/time" && method == Method::GET {
        return Ok(json_response(StatusCode::OK, state.build_player_time()));
    }

    if pathname == "/stage/player/queue" && method == Method::GET {
        return Ok(json_response(
            StatusCode::OK,
            state.build_player_queue_window(query.as_deref()),
        ));
    }

    if pathname == "/stage/state" && method == Method::DELETE {
        let status = state.clear_state();
        return Ok(json_response(StatusCode::OK, status));
    }

    if pathname == "/stage/lyrics" && method == Method::POST {
        let payload = read_json_body(
            req,
            "Failed to parse Stage lyrics JSON payload.",
            "INVALID_STAGE_LYRICS_JSON",
        )
        .await?;
        let session = normalize_lyrics_session_payload(&payload)?;
        state.set_lyrics_session(session);
        return Ok(json_response(StatusCode::OK, state.build_status()));
    }

    if pathname == "/stage/session" && method == Method::POST {
        let working_session_id = format!("stage-{}-{}", now_ms(), random_uuid());
        let working_directory = state.core.sessions_root.join(&working_session_id);
        tokio::fs::create_dir_all(&working_directory)
            .await
            .map_err(|e| {
                StageError::new(
                    500,
                    "STAGE_INTERNAL_ERROR",
                    format!("failed to create session directory: {e}"),
                )
            })?;
        let is_multipart = req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("multipart/form-data"))
            .unwrap_or(false);
        let result = if is_multipart {
            let multipart = axum::extract::Multipart::from_request(req, &())
                .await
                .map_err(|_| {
                    StageError::validation(
                        "Failed to parse Stage multipart payload.",
                        "INVALID_STAGE_MULTIPART",
                    )
                })?;
            let parsed = parse_stage_multipart(multipart, &working_directory).await?;
            state
                .create_media_session_from_multipart(parsed, &working_directory)
                .await
        } else {
            let body = read_body_bytes(req).await?;
            let payload = serde_json::from_slice::<Value>(&body).map_err(|e| {
                StageError::new(
                    400,
                    "INVALID_STAGE_JSON",
                    format!("Failed to parse Stage JSON payload: {e}"),
                )
            })?;
            state
                .create_media_session_from_json(&payload, &working_directory)
                .await
        };
        match result {
            Ok(session_result) => {
                state.set_media_session(session_result);
                return Ok(json_response(StatusCode::OK, state.build_status()));
            }
            Err(err) => {
                let dir = working_directory;
                tokio::fs::remove_dir_all(&dir).await.ok();
                return Err(err);
            }
        }
    }

    if pathname == "/stage/player/search" && method == Method::POST {
        let payload = read_json_body(
            req,
            "Failed to parse Stage player search JSON payload.",
            "INVALID_STAGE_PLAYER_SEARCH_JSON",
        )
        .await?;
        return handle_search(payload, &state, false).await;
    }

    if pathname == "/stage/search" && method == Method::POST {
        let payload = read_json_body(
            req,
            "Failed to parse Stage player search JSON payload.",
            "INVALID_STAGE_PLAYER_SEARCH_JSON",
        )
        .await?;
        return handle_search(payload, &state, true).await;
    }

    if pathname == "/stage/player/play" && method == Method::POST {
        let payload = read_json_body(
            req,
            "Failed to parse Stage player play JSON payload.",
            "INVALID_STAGE_PLAYER_PLAY_JSON",
        )
        .await?;
        return handle_play(payload, &state, false).await;
    }

    if pathname == "/stage/play" && method == Method::POST {
        let payload = read_json_body(
            req,
            "Failed to parse Stage player play JSON payload.",
            "INVALID_STAGE_PLAYER_PLAY_JSON",
        )
        .await?;
        return handle_play(payload, &state, true).await;
    }

    if pathname == "/stage/player/control" && method == Method::POST {
        let payload = read_json_body(
            req,
            "Failed to parse Stage player control JSON payload.",
            "INVALID_STAGE_PLAYER_CONTROL_JSON",
        )
        .await?;
        return handle_control(payload, &state).await;
    }

    if pathname == "/stage/player/queue" && method == Method::POST {
        let payload = read_json_body(
            req,
            "Failed to parse Stage player queue JSON payload.",
            "INVALID_STAGE_PLAYER_QUEUE_JSON",
        )
        .await?;
        return handle_queue(payload, &state).await;
    }

    Ok(json_response(
        StatusCode::NOT_FOUND,
        json!({ "error": "Not found." }),
    ))
}

async fn handle_search(
    payload: Value,
    state: &StageState,
    deprecated: bool,
) -> Result<Response<Body>, StageError> {
    let query = normalize_stage_text(payload.get("query").unwrap_or(&Value::Null));
    if query.is_empty() {
        return Err(StageError::validation(
            "Stage player search query is required.",
            "INVALID_STAGE_PLAYER_SEARCH_QUERY",
        ));
    }
    let limit = payload
        .get("limit")
        .and_then(Value::as_f64)
        .map(|v| (v.floor() as i64).clamp(1, 50))
        .unwrap_or(10);
    let songs = state.search_songs(&query, limit).await?;
    let mut body = json!({
        "domain": "player-playback",
        "direction": "outside-in",
        "query": query,
        "songs": songs,
    });
    if deprecated {
        body["deprecated"] = json!(true);
        body["replacement"] = json!("/stage/player/search");
    }
    Ok(json_response(StatusCode::OK, body))
}

async fn handle_play(
    payload: Value,
    state: &StageState,
    deprecated: bool,
) -> Result<Response<Body>, StageError> {
    let song_id = payload.get("songId").and_then(Value::as_i64);
    let append_to_queue = payload
        .get("appendToQueue")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(song_id) = song_id else {
        return Err(StageError::validation(
            "Stage player play payload requires a positive integer songId.",
            "INVALID_STAGE_PLAYER_PLAY_SONG_ID",
        ));
    };
    if song_id <= 0 {
        return Err(StageError::validation(
            "Stage player play payload requires a positive integer songId.",
            "INVALID_STAGE_PLAYER_PLAY_SONG_ID",
        ));
    }
    let result = state.request_song_play(song_id, append_to_queue).await?;
    let mut body = json!({
        "domain": "player-playback",
        "direction": "outside-in",
        "ok": true,
        "songId": song_id,
        "appendToQueue": append_to_queue,
    });
    if deprecated {
        body["deprecated"] = json!(true);
        body["replacement"] = json!("/stage/player/play");
    }
    merge_result_fields(&mut body, &result);
    Ok(json_response(StatusCode::OK, body))
}

async fn handle_control(payload: Value, state: &StageState) -> Result<Response<Body>, StageError> {
    let action = normalize_stage_text(payload.get("action").unwrap_or(&Value::Null));
    let status = ensure_control_allowed(state, &action)?;
    let mut control_payload = json!({ "action": action.clone() });
    if action == "seek" {
        let position_ms =
            normalize_stage_integer(payload.get("positionMs").unwrap_or(&Value::Null), -1);
        if position_ms < 0 {
            return Err(StageError::validation(
                "Stage player seek requires a non-negative positionMs.",
                "INVALID_STAGE_PLAYER_SEEK_POSITION",
            ));
        }
        control_payload["positionMs"] = json!(position_ms);
    }
    let _ = state.request_player_control(&control_payload).await?;
    Ok(json_response(
        StatusCode::OK,
        json!({
            "domain": "player-playback",
            "direction": "outside-in",
            "accepted": true,
            "action": action,
            "playbackContext": status.get("playbackContext"),
        }),
    ))
}

async fn handle_queue(payload: Value, state: &StageState) -> Result<Response<Body>, StageError> {
    let action = normalize_stage_text(payload.get("action").unwrap_or(&Value::Null));
    let status = ensure_queue_allowed(state, &action)?;
    let mut op_payload = json!({ "action": action.clone() });
    if let Some(v) = payload.get("songId") {
        if let Some(id) = v.as_i64() {
            op_payload["songId"] = json!(id);
        }
    }
    if let Some(v) = payload.get("songIds") {
        if let Some(arr) = v.as_array() {
            let ids: Vec<i64> = arr
                .iter()
                .filter_map(Value::as_i64)
                .filter(|id| *id > 0)
                .collect();
            if !ids.is_empty() {
                op_payload["songIds"] = json!(ids);
            }
        }
    }
    for key in ["queueItemId", "fromQueueItemId"] {
        let value = normalize_stage_text(payload.get(key).unwrap_or(&Value::Null));
        if !value.is_empty() {
            op_payload[key] = json!(value);
        }
    }
    for key in ["fromIndex", "toIndex", "index"] {
        if let Some(v) = payload.get(key) {
            if let Some(id) = v.as_i64() {
                op_payload[key] = json!(id);
            }
        }
    }
    let result = state.request_player_queue(&op_payload).await?;
    let snapshot = state.current_player_snapshot_or_fallback();
    let mut body = json!({
        "domain": "player-playback",
        "direction": "outside-in",
        "accepted": true,
        "action": action,
        "playbackContext": status.get("playbackContext"),
        "queue": queue_summary(&snapshot),
    });
    merge_result_fields(&mut body, &result);
    Ok(json_response(StatusCode::OK, body))
}

fn merge_result_fields(body: &mut Value, result: &Value) {
    for key in ["changed", "deduplicated", "affectedCount"] {
        if let Some(v) = result.get(key) {
            body[key] = v.clone();
        }
    }
    if let Some(v) = result.get("diff") {
        if v.is_object() {
            body["diff"] = v.clone();
        }
    }
}

fn ensure_control_allowed(state: &StageState, action: &str) -> Result<Value, StageError> {
    let capability_key = PLAYER_CONTROL_CAPABILITY
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, key)| *key)
        .ok_or_else(|| {
            StageError::validation(
                "Unsupported Stage player control action.",
                "INVALID_STAGE_PLAYER_CONTROL_ACTION",
            )
            .with_details(json!({ "action": action }))
        })?;
    let status = state.build_player_status();
    let allowed = status
        .get("controlCapabilities")
        .and_then(|c| c.get(capability_key))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !allowed {
        return Err(StageError::new(
            409,
            "STAGE_PLAYER_CONTROL_UNSUPPORTED",
            "Control action is not supported in the current playback context.",
        )
        .with_details(json!({
            "action": action,
            "playbackContext": status.get("playbackContext"),
        })));
    }
    Ok(status)
}

fn ensure_queue_allowed(state: &StageState, action: &str) -> Result<Value, StageError> {
    let capability_key = PLAYER_QUEUE_CAPABILITY
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, key)| *key)
        .ok_or_else(|| {
            StageError::validation(
                "Unsupported Stage player queue action.",
                "INVALID_STAGE_PLAYER_QUEUE_ACTION",
            )
            .with_details(json!({ "action": action }))
        })?;
    let status = state.build_player_status();
    let allowed = status
        .get("queueCapabilities")
        .and_then(|c| c.get(capability_key))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !allowed {
        return Err(StageError::new(
            409,
            "STAGE_PLAYER_QUEUE_UNSUPPORTED",
            "Queue action is not supported in the current playback context.",
        )
        .with_details(json!({
            "action": action,
            "playbackContext": status.get("playbackContext"),
        })));
    }
    Ok(status)
}

async fn read_body_bytes(req: Request<Body>) -> Result<Vec<u8>, StageError> {
    let body = axum::body::to_bytes(req.into_body(), STAGE_JSON_BODY_LIMIT_BYTES)
        .await
        .map_err(|_| {
            StageError::new(
                413,
                "STAGE_BODY_TOO_LARGE",
                "Stage request body exceeded the size limit.",
            )
            .with_details(json!({ "maxBytes": STAGE_JSON_BODY_LIMIT_BYTES }))
        })?;
    Ok(body.to_vec())
}

async fn read_json_body(
    req: Request<Body>,
    error_message: &str,
    error_code: &str,
) -> Result<Value, StageError> {
    let body = read_body_bytes(req).await?;
    let text = String::from_utf8_lossy(&body);
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|e| StageError::new(400, error_code, format!("{error_message}: {e}")))
}

fn req_range(headers: &axum::http::header::HeaderMap) -> Option<String> {
    headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
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

async fn serve_stage_file(
    _req: Request<Body>,
    path: &Path,
    content_type: &str,
    range: Option<String>,
) -> Result<Response<Body>, StageError> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    let file = tokio::fs::File::open(path).await.map_err(|e| {
        StageError::new(
            500,
            "STAGE_INTERNAL_ERROR",
            format!("failed to open stage file: {e}"),
        )
    })?;
    let file_len = file
        .metadata()
        .await
        .map_err(|e| {
            StageError::new(
                500,
                "STAGE_INTERNAL_ERROR",
                format!("failed to stat stage file: {e}"),
            )
        })?
        .len();

    let Some(range) = range else {
        let stream = tokio_util::io::ReaderStream::new(file);
        let mut resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type)
            .header("Accept-Ranges", "bytes")
            .header("Access-Control-Allow-Origin", "*")
            .header(
                "Access-Control-Allow-Headers",
                "Authorization, Content-Type, Range",
            )
            .header("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS")
            .body(Body::from_stream(stream))
            .expect("valid stage file response");
        resp.headers_mut().insert("Content-Length", file_len.into());
        return Ok(resp);
    };

    let parsed = parse_byte_range(&range, file_len);
    let Some((start, end)) = parsed else {
        return Ok(json_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            json!({ "error": "Invalid byte range." }),
        ));
    };
    if start > end || start >= file_len {
        return Ok(json_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            json!({ "error": "Requested range is outside the file." }),
        ));
    }

    let mut file = file;
    file.seek(SeekFrom::Start(start)).await.map_err(|e| {
        StageError::new(
            500,
            "STAGE_INTERNAL_ERROR",
            format!("failed to seek stage file: {e}"),
        )
    })?;
    let limited = file.take(end - start + 1);
    let stream = tokio_util::io::ReaderStream::new(limited);
    let mut resp = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("Content-Type", content_type)
        .header("Accept-Ranges", "bytes")
        .header("Content-Range", format!("bytes {start}-{end}/{file_len}"))
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type, Range",
        )
        .header("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS")
        .body(Body::from_stream(stream))
        .expect("valid stage ranged response");
    resp.headers_mut()
        .insert("Content-Length", (end - start + 1).into());
    Ok(resp)
}

fn parse_byte_range(header: &str, file_len: u64) -> Option<(u64, u64)> {
    let lower = header.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("bytes=")?;
    let (a, b) = rest.split_once('-')?;
    let start = if a.trim().is_empty() {
        0
    } else {
        a.trim().parse::<u64>().ok()?
    };
    let end = if b.trim().is_empty() {
        file_len.saturating_sub(1)
    } else {
        b.trim().parse::<u64>().ok()?
    };
    Some((start, end))
}

fn send_binary(buffer: &[u8], content_type: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type",
        )
        .header("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS")
        .body(Body::from(buffer.to_vec()))
        .expect("valid stage binary response")
}

// ---------------------------------------------------------------------------
// Multipart parsing (audioFile / lyricsFile / coverFile + text fields)
// ---------------------------------------------------------------------------

async fn parse_stage_multipart(
    mut multipart: axum::extract::Multipart,
    working_directory: &Path,
) -> Result<MultipartPayload, StageError> {
    use tokio::io::AsyncWriteExt;
    use tokio_stream::StreamExt as _;

    let mut fields: HashMap<String, String> = HashMap::new();
    let mut files: HashMap<String, UploadedFile> = HashMap::new();
    let mut part_count = 0usize;
    let mut file_count = 0usize;
    let mut field_count = 0usize;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        StageError::validation(
            "Failed to parse Stage multipart payload.",
            "INVALID_STAGE_MULTIPART",
        )
        .with_details(json!({ "reason": e.to_string() }))
    })? {
        part_count += 1;
        if part_count > STAGE_MULTIPART_PART_COUNT_LIMIT {
            return Err(StageError::new(
                413,
                "STAGE_MULTIPART_TOO_LARGE",
                "Stage multipart request exceeded the part count limit.",
            ));
        }
        let name = field.name().unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue;
        }
        if let Some(file_name) = field.file_name() {
            file_count += 1;
            if file_count > STAGE_MULTIPART_FILE_COUNT_LIMIT {
                return Err(StageError::new(
                    413,
                    "STAGE_MULTIPART_TOO_LARGE",
                    "Stage multipart request exceeded the file count limit.",
                ));
            }
            let safe_base = if file_name.trim().is_empty() {
                name.clone()
            } else {
                let base = Path::new(file_name)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&name)
                    .replace(
                        |c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '_',
                        "_",
                    );
                if base.is_empty() {
                    name.clone()
                } else {
                    base
                }
            };
            let file_path = working_directory.join(format!("{name}-{}-{safe_base}", now_ms()));
            let content_type = field.content_type().map(|s| s.to_string());
            let mut total: u64 = 0;
            let mut file = tokio::fs::File::create(&file_path).await.map_err(|e| {
                StageError::new(
                    500,
                    "STAGE_INTERNAL_ERROR",
                    format!("failed to create upload file: {e}"),
                )
            })?;
            let mut field = field;
            while let Some(chunk) = field.next().await {
                let chunk = chunk.map_err(|e| {
                    StageError::new(
                        400,
                        "INVALID_STAGE_MULTIPART",
                        format!("multipart read failed: {e}"),
                    )
                })?;
                total += chunk.len() as u64;
                if total > STAGE_MULTIPART_FILE_LIMIT_BYTES {
                    return Err(StageError::new(
                        413,
                        "STAGE_FILE_TOO_LARGE",
                        format!("Multipart file {name} exceeded the size limit."),
                    )
                    .with_details(
                        json!({ "fieldName": name, "maxBytes": STAGE_MULTIPART_FILE_LIMIT_BYTES }),
                    ));
                }
                file.write_all(&chunk).await.map_err(|e| {
                    StageError::new(
                        500,
                        "STAGE_INTERNAL_ERROR",
                        format!("failed to write upload file: {e}"),
                    )
                })?;
            }
            file.flush().await.ok();
            drop(file);
            files.insert(
                name.clone(),
                UploadedFile {
                    content_type,
                    file_path,
                },
            );
        } else {
            field_count += 1;
            if field_count > STAGE_MULTIPART_FIELD_COUNT_LIMIT {
                return Err(StageError::new(
                    413,
                    "STAGE_MULTIPART_TOO_LARGE",
                    "Stage multipart request exceeded the field count limit.",
                ));
            }
            let mut buf: Vec<u8> = Vec::new();
            let mut field = field;
            while let Some(chunk) = field.next().await {
                let chunk = chunk.map_err(|e| {
                    StageError::new(
                        400,
                        "INVALID_STAGE_MULTIPART",
                        format!("multipart read failed: {e}"),
                    )
                })?;
                if buf.len() as u64 + chunk.len() as u64 > STAGE_MULTIPART_FIELD_LIMIT_BYTES {
                    return Err(StageError::new(
                        413,
                        "STAGE_MULTIPART_TOO_LARGE",
                        "Stage multipart field exceeded the size limit.",
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            fields.insert(name, String::from_utf8_lossy(&buf).into_owned());
        }
    }

    Ok(MultipartPayload { fields, files })
}

// ---------------------------------------------------------------------------
// Server lifecycle (kept alive in managed state)
// ---------------------------------------------------------------------------

/// Owns the tokio runtime that drives the axum server while Stage mode is
/// enabled. Dropping the server releases the port.
pub struct StageApiServer {
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for StageApiServer {
    // Dropping a tokio Runtime blocks and panics in async contexts, so move it
    // to a plain thread; joining that thread keeps the loopback port closed
    // before `stop_server()` / `set_enabled(false)` returns (the disabled-state
    // tests assert the port is immediately unreachable). The runtime is
    // dedicated: its workers never run the code that drops it.
    fn drop(&mut self) {
        if let Some(runtime) = self._runtime.take() {
            std::thread::spawn(move || drop(runtime))
                .join()
                .expect("stage runtime shutdown panicked");
        }
    }
}

impl StageApiServer {
    fn start(state: StageState) -> Result<Self, String> {
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
                    .map_err(|e| format!("failed to bind stage server on 127.0.0.1:{port}: {e}"))?;
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, router).await {
                        eprintln!("[stage] server stopped: {e}");
                    }
                });
                Ok::<(), String>(())
            })
            .map_err(|e| e.to_string())?;
        eprintln!("[stage] API server running on http://127.0.0.1:{port}");
        Ok(Self {
            _runtime: Some(runtime),
        })
    }
}

// ---------------------------------------------------------------------------
// Tauri commands (snake_case, shim-invoked)
// ---------------------------------------------------------------------------

#[tauri::command]
// 返回 Stage API 状态（enabled/modeEnabled/source/port/token/会话），形状同 buildStageStatus()。
pub fn stage_get_status(state: tauri::State<'_, StageState>) -> Result<Value, String> {
    Ok(state.build_status())
}

#[tauri::command]
// 持久化 Stage 模式开关并按状态同步启动/停止回环服务器。
pub fn stage_set_enabled(
    app: AppHandle,
    enabled: bool,
    state: tauri::State<'_, StageState>,
) -> Result<Value, String> {
    let settings = app.state::<SettingsStore>();
    state.set_enabled(enabled, &settings)
}

#[tauri::command]
// 重新生成 Stage bearer token（持久化 + 关闭已连接的 WebSocket）。
pub fn stage_regenerate_token(
    app: AppHandle,
    state: tauri::State<'_, StageState>,
) -> Result<Value, String> {
    let settings = app.state::<SettingsStore>();
    state.regenerate_token(&settings)
}

#[tauri::command]
// 清空当前 Stage 会话（lyrics/media）并广播清除事件。
pub fn stage_clear_state(state: tauri::State<'_, StageState>) -> Result<Value, String> {
    Ok(state.clear_state())
}

#[tauri::command]
// 渲染进程完成一次外部播放请求（requestId/ok/error/snapshot/result）。
pub fn stage_complete_external_play(
    result: Value,
    state: tauri::State<'_, StageState>,
) -> Result<bool, String> {
    Ok(state.complete_external_play(&result))
}

#[tauri::command]
// 发布播放器快照；按 track/queue/playback 键变化广播 WS 事件。
pub fn stage_publish_player_snapshot(
    snapshot: Value,
    options: Option<Value>,
    state: tauri::State<'_, StageState>,
) -> Result<Value, String> {
    Ok(state.publish_player_snapshot(snapshot, options))
}

#[tauri::command]
// 渲染进程完成一次播放器控制请求。
pub fn stage_complete_player_control(
    result: Value,
    state: tauri::State<'_, StageState>,
) -> Result<bool, String> {
    Ok(state.complete_player_control(&result))
}

#[tauri::command]
// 渲染进程完成一次播放器队列请求。
pub fn stage_complete_player_queue(
    result: Value,
    state: tauri::State<'_, StageState>,
) -> Result<bool, String> {
    Ok(state.complete_player_queue(&result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener as StdTcpListener;

    fn free_port() -> u16 {
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "folia-stage-test-{}",
            now_ms() + rand::random::<i64>() % 1000
        ))
    }

    fn test_store(root: &Path) -> SettingsStore {
        SettingsStore::open(root).unwrap()
    }

    fn test_context() -> (StageState, SettingsStore, PathBuf) {
        let root = temp_root();
        let store = test_store(&root);
        let port = free_port();
        store
            .set(STAGE_API_PORT_KEY.to_string(), json!(port))
            .unwrap();
        let emit: EmitFn = Arc::new(|_channel, _payload| {});
        let netease_port: PortFn = Arc::new(|| 0);
        let state = StageState::for_test(root.clone().join("stage"), emit, netease_port);
        (state, store, root)
    }

    fn enable(context: &(StageState, SettingsStore, PathBuf)) -> (String, u16) {
        let (state, store, root) = context;
        let status = enable_on_thread(state.clone(), &root);
        let _ = store;
        let token = status["token"].as_str().unwrap().to_string();
        let port = status["port"].as_u64().unwrap() as u16;
        (token, port)
    }

    // StageApiServer owns its own tokio runtime and calls `block_on` during
    // bind, so the enable step must run off the test runtime's threads. After
    // enabling we probe the listener until it accepts (the first hyper client
    // request can otherwise race the axum accept loop on Windows).
    fn enable_on_thread(state: StageState, root: &Path) -> Value {
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
            let port = port as u16;
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        status
    }

    fn auth(token: &str) -> String {
        format!("Bearer {token}")
    }

    fn normal_snapshot() -> Value {
        json!({
            "playbackContext": "normal-playback",
            "current": {
                "id": "42",
                "source": "netease",
                "title": "String Theocracy",
                "artist": "Mili",
                "album": "Library Of Ruina",
                "durationMs": 188000,
                "coverUrl": "https://example.com/cover.jpg",
            },
            "playerState": "PLAYING",
            "positionMs": 1000,
            "durationMs": 188000,
            "sampledAtMs": 0,
            "updatedAt": 0,
            "controlCapabilities": {
                "play": true, "pause": true, "resume": true,
                "seek": true, "previous": true, "next": true,
            },
            "queueCapabilities": {
                "append": true, "insertNext": true, "remove": true,
                "move": true, "select": true, "clear": true,
            },
            "queue": {
                "currentIndex": 0,
                "items": [{
                    "queueItemId": "netease:42:0",
                    "id": "42",
                    "source": "netease",
                    "title": "String Theocracy",
                    "artist": "Mili",
                    "album": "Library Of Ruina",
                    "durationMs": 188000,
                    "coverUrl": "https://example.com/cover.jpg",
                }],
            },
        })
    }

    async fn json_client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[test]
    fn constant_time_token_eq_is_case_sensitive_and_secure() {
        assert!(constant_time_token_eq("abc123", "abc123"));
        assert!(!constant_time_token_eq("abc123", "abc124"));
        assert!(!constant_time_token_eq("abc123", "ABC123"));
        assert!(!constant_time_token_eq("abc123", "abc1234"));
        assert!(!constant_time_token_eq("", "abc"));
    }

    #[test]
    fn stage_mode_source_resolution() {
        let root = temp_root();
        let store = test_store(&root);
        let emit: EmitFn = Arc::new(|_, _| {});
        let state = StageState::for_test(root.clone().join("stage"), emit, Arc::new(|| 0));
        state.sync_from_settings(&store);
        assert_eq!(state.stage_source(), "stage-api");
        assert!(!state.is_mode_enabled());
        assert!(!state.is_stage_enabled());

        store
            .set(STAGE_MODE_ENABLED_KEY.to_string(), json!(true))
            .unwrap();
        store
            .set(STAGE_MODE_SOURCE_KEY.to_string(), json!("now-playing"))
            .unwrap();
        state.sync_from_settings(&store);
        assert_eq!(state.stage_source(), "now-playing");
        assert!(!state.is_stage_enabled()); // now-playing is not stage-api

        store
            .set(STAGE_MODE_SOURCE_KEY.to_string(), json!("stage-api"))
            .unwrap();
        state.sync_from_settings(&store);
        assert!(state.is_stage_enabled());
    }

    #[test]
    fn normalize_snapshot_shapes() {
        let snapshot = normalize_player_snapshot(&normal_snapshot());
        assert_eq!(snapshot["playerState"], "PLAYING");
        assert_eq!(snapshot["current"]["title"], "String Theocracy");
        assert_eq!(snapshot["queue"]["length"], 1);
        assert_eq!(snapshot["controlCapabilities"]["previous"], true);
        assert_eq!(snapshot["queueCapabilities"]["append"], true);

        // unknown playerState coerces to IDLE; missing queue coerces to empty.
        let mut weird = normal_snapshot();
        weird["playerState"] = json!("BOGUS");
        weird.as_object_mut().unwrap().remove("queue");
        let snapshot = normalize_player_snapshot(&weird);
        assert_eq!(snapshot["playerState"], "IDLE");
        assert_eq!(snapshot["queue"]["items"], json!([]));
        assert_eq!(snapshot["queue"]["currentIndex"], -1);
    }

    #[test]
    fn control_capabilities_respect_context() {
        // external-playback-source disables transport controls even with a current track.
        let snapshot = normalize_player_snapshot(&json!({
            "playbackContext": "external-playback-source",
            "current": { "id": "1", "title": "T" },
        }));
        assert_eq!(snapshot["controlCapabilities"]["play"], false);
        assert_eq!(snapshot["controlCapabilities"]["seek"], false);
        // normal-playback with no current disables transport too.
        let snapshot = normalize_player_snapshot(&json!({
            "playbackContext": "normal-playback",
        }));
        assert_eq!(snapshot["controlCapabilities"]["play"], false);
        assert_eq!(snapshot["queueCapabilities"]["append"], true);
    }

    #[tokio::test]
    async fn health_status_and_auth_contract() {
        let context = test_context();
        let (token, port) = enable(&context);
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;

        // health is unauthenticated
        let res = client
            .get(format!("{base}/stage/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let health: Value = res.json().await.unwrap();
        assert_eq!(health["enabled"], true);
        assert_eq!(health["port"], port);

        // status requires auth
        let res = client
            .get(format!("{base}/stage/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = client
            .get(format!("{base}/stage/status"))
            .header("Authorization", auth("wrong-token"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = client
            .get(format!("{base}/stage/status"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let status: Value = res.json().await.unwrap();
        assert_eq!(status["domain"], "stage-input");
        assert_eq!(status["direction"], "outside-in");
        assert_eq!(status["enabled"], true);
        assert_eq!(status["modeEnabled"], true);
        assert_eq!(status["source"], "stage-api");
        assert_eq!(status["activeEntryKind"], Value::Null);
        assert_eq!(status["lyricsSession"], Value::Null);
        assert_eq!(status["mediaSession"], Value::Null);

        // CORS preflight
        let res = client
            .request(Method::OPTIONS, format!("{base}/stage/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            res.headers()["access-control-allow-origin"]
                .to_str()
                .unwrap(),
            "*"
        );

        // disable stops the loopback server entirely
        let (state, store, _) = &context;
        state.set_enabled(false, store).unwrap();
        let res = client.get(format!("{base}/stage/health")).send().await;
        assert!(
            res.is_err(),
            "server should be stopped when stage mode is disabled"
        );
    }

    #[tokio::test]
    async fn lyrics_session_contract() {
        let context = test_context();
        let (token, port) = enable(&context);
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;

        let res = client
            .post(format!("{base}/stage/lyrics"))
            .header("Authorization", auth(&token))
            .json(&json!({
                "title": "Stage Lyrics",
                "artist": "Folia",
                "lyricSource": {
                    "type": "local",
                    "lrcContent": "[00:00.00]Hello world",
                    "tLrcContent": "[00:00.00]你好，世界",
                    "formatHint": "lrc",
                },
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["activeEntryKind"], "lyrics");
        assert_eq!(payload["lyricsSession"]["title"], "Stage Lyrics");
        assert_eq!(payload["lyricsSession"]["lyricSource"]["type"], "local");
        assert_eq!(
            payload["lyricsSession"]["lyricSource"]["lrcContent"],
            "[00:00.00]Hello world"
        );
        assert_eq!(payload["mediaSession"], Value::Null);

        // invalid lyric source -> 400 INVALID_STAGE_LYRICS
        let res = client
            .post(format!("{base}/stage/lyrics"))
            .header("Authorization", auth(&token))
            .json(&json!({ "title": "Bad" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["code"], "INVALID_STAGE_LYRICS");

        // unsupported format hint is dropped, not rejected
        let res = client
            .post(format!("{base}/stage/lyrics"))
            .header("Authorization", auth(&token))
            .json(&json!({
                "lyricSource": { "type": "local", "lrcContent": "[00:00.00]X", "formatHint": "bogus" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = res.json().await.unwrap();
        assert!(body["lyricsSession"]["lyricSource"]
            .get("formatHint")
            .is_none());

        let (state, store, root) = &context;
        state.set_enabled(false, store).unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn media_session_json_and_clear_contract() {
        let context = test_context();
        let (token, port) = enable(&context);
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;

        let res = client
            .post(format!("{base}/stage/session"))
            .header("Authorization", auth(&token))
            .json(&json!({
                "title": "Example",
                "artist": "Artist",
                "audioUrl": "https://example.com/demo.mp3",
                "lyricsText": "[00:00.00]Hello",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["activeEntryKind"], "media");
        assert_eq!(payload["mediaSession"]["title"], "Example");
        assert_eq!(payload["mediaSession"]["artist"], "Artist");
        assert_eq!(
            payload["mediaSession"]["audioUrl"],
            "https://example.com/demo.mp3"
        );
        assert_eq!(
            payload["mediaSession"]["audioSrc"],
            "https://example.com/demo.mp3"
        );
        assert_eq!(payload["mediaSession"]["lyricsFormat"], "lrc");
        assert!(payload["mediaSession"]["id"]
            .as_str()
            .unwrap()
            .starts_with("stage-"));

        // invalid audio source
        let res = client
            .post(format!("{base}/stage/session"))
            .header("Authorization", auth(&token))
            .json(&json!({ "title": "No Audio" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["code"], "INVALID_AUDIO_SOURCE");

        // DELETE /stage/state clears the session
        let res = client
            .delete(format!("{base}/stage/state"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["activeEntryKind"], Value::Null);
        assert_eq!(payload["lyricsSession"], Value::Null);
        assert_eq!(payload["mediaSession"], Value::Null);

        let (state, store, root) = &context;
        state.set_enabled(false, store).unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn oversize_json_body_rejected() {
        let context = test_context();
        let (token, port) = enable(&context);
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;
        let big = "x".repeat((STAGE_JSON_BODY_LIMIT_BYTES + 10) * 2);
        let res = client
            .post(format!("{base}/stage/lyrics"))
            .header("Authorization", auth(&token))
            .header("Content-Type", "application/json")
            .body(json!({ "lyricSource": { "type": "local", "lrcContent": big } }).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let (state, store, root) = &context;
        state.set_enabled(false, store).unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn player_status_time_and_queue_window() {
        let context = test_context();
        let (token, port) = enable(&context);
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;
        let (state, store, root) = &context;

        state.publish_player_snapshot(normal_snapshot(), None);

        let res = client
            .get(format!("{base}/stage/player/status"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["domain"], "player-playback");
        assert_eq!(payload["direction"], "inside-out");
        assert_eq!(payload["playbackContext"], "normal-playback");
        assert_eq!(payload["current"]["id"], "42");
        assert_eq!(payload["controlCapabilities"]["next"], true);
        assert_eq!(payload["controlCapabilities"]["previous"], true);
        assert_eq!(payload["queue"]["currentIndex"], 0);
        assert_eq!(payload["queue"]["length"], 1);
        assert!(payload["queue"].get("items").is_none());
        assert!(payload["queue"]["revision"].as_str().unwrap().len() == 40);

        // time compensation only while PLAYING
        let now = now_ms();
        state.publish_player_snapshot(
            json!({
                "playbackContext": "normal-playback",
                "current": { "id": "42", "title": "T" },
                "playerState": "PLAYING",
                "positionMs": 1000,
                "durationMs": 10000,
                "sampledAtMs": now - 5000,
                "updatedAt": now,
            }),
            None,
        );
        let res = client
            .get(format!("{base}/stage/player/time"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        let payload: Value = res.json().await.unwrap();
        assert!(payload["positionMs"].as_i64().unwrap() >= 5500);
        assert!(payload["positionMs"].as_i64().unwrap() <= 10000);

        state.publish_player_snapshot(
            json!({
                "playbackContext": "normal-playback",
                "playerState": "PAUSED",
                "positionMs": 2000,
                "durationMs": 10000,
                "sampledAtMs": now - 5000,
                "updatedAt": now,
            }),
            None,
        );
        let res = client
            .get(format!("{base}/stage/player/time"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["playerState"], "PAUSED");
        assert_eq!(payload["positionMs"], 2000);

        // queue window
        let items: Vec<Value> = (0..3)
            .map(|i| {
                json!({
                    "queueItemId": format!("netease:{}:{i}", 42 + i),
                    "id": format!("{}", 42 + i),
                    "source": "netease",
                    "title": format!("Track {}", i + 1),
                    "artist": "Folia",
                    "album": "Stage",
                    "durationMs": 180000 + i,
                    "coverUrl": Value::Null,
                })
            })
            .collect();
        state.publish_player_snapshot(
            json!({
                "playbackContext": "normal-playback",
                "playerState": "IDLE",
                "queue": { "currentIndex": 1, "items": items },
            }),
            None,
        );
        let res = client
            .get(format!("{base}/stage/player/queue?offset=1&limit=1"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["queue"]["offset"], 1);
        assert_eq!(payload["queue"]["limit"], 1);
        assert_eq!(payload["queue"]["returned"], 1);
        assert_eq!(payload["queue"]["hasMore"], true);
        assert_eq!(payload["queue"]["nextOffset"], 2);
        assert_eq!(payload["queue"]["items"][0]["id"], "43");
        assert_eq!(payload["queue"]["length"], 3);

        let res = client
            .get(format!("{base}/stage/player/queue?offset=99&limit=1"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["queue"]["offset"], 3);
        assert_eq!(payload["queue"]["returned"], 0);
        assert_eq!(payload["queue"]["hasMore"], false);
        assert_eq!(payload["queue"]["items"], json!([]));

        state.set_enabled(false, store).unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn player_control_and_queue_requests() {
        let root = temp_root();
        let store = test_store(&root);
        let port = free_port();
        store
            .set(STAGE_API_PORT_KEY.to_string(), json!(port))
            .unwrap();

        let holder: Arc<Mutex<Option<StageState>>> = Arc::new(Mutex::new(None));
        let controls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let queues: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let holder_emit = holder.clone();
        let controls_emit = controls.clone();
        let queues_emit = queues.clone();
        let emit: EmitFn = Arc::new(move |channel, payload| {
            if channel == "stage-player-control-request" {
                controls_emit.lock().unwrap().push(payload.clone());
                if let Some(state) = holder_emit.lock().unwrap().clone() {
                    let rid = payload["requestId"].as_str().unwrap().to_string();
                    let s = state.clone();
                    std::thread::spawn(move || {
                        let _ = s.complete_player_control(&json!({ "requestId": rid, "ok": true }));
                    });
                }
            }
            if channel == "stage-player-queue-request" {
                queues_emit.lock().unwrap().push(payload.clone());
                if let Some(state) = holder_emit.lock().unwrap().clone() {
                    let rid = payload["requestId"].as_str().unwrap().to_string();
                    let s = state.clone();
                    std::thread::spawn(move || {
                        let _ = s.complete_player_queue(&json!({ "requestId": rid, "ok": true }));
                    });
                }
            }
        });
        let state = StageState::for_test(root.clone().join("stage"), emit, Arc::new(|| 0));
        *holder.lock().unwrap() = Some(state.clone());
        let status = enable_on_thread(state.clone(), &root);
        let token = status["token"].as_str().unwrap().to_string();
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;

        // unsupported action -> 409 (known action, but no capability yet)
        let res = client
            .post(format!("{base}/stage/player/control"))
            .header("Authorization", auth(&token))
            .json(&json!({ "action": "nonsense" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = client
            .post(format!("{base}/stage/player/control"))
            .header("Authorization", auth(&token))
            .json(&json!({ "action": "next" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        state.publish_player_snapshot(normal_snapshot(), None);

        // invalid seek -> 400
        let res = client
            .post(format!("{base}/stage/player/control"))
            .header("Authorization", auth(&token))
            .json(&json!({ "action": "seek" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["code"], "INVALID_STAGE_PLAYER_SEEK_POSITION");
        assert!(controls.lock().unwrap().is_empty());

        // valid seek
        let res = client
            .post(format!("{base}/stage/player/control"))
            .header("Authorization", auth(&token))
            .json(&json!({ "action": "seek", "positionMs": 5000 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["accepted"], true);
        assert_eq!(payload["action"], "seek");
        assert_eq!(controls.lock().unwrap()[0]["positionMs"], 5000);

        // queue GET + POST
        let res = client
            .get(format!("{base}/stage/player/queue?offset=0&limit=1"))
            .header("Authorization", auth(&token))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.json::<Value>().await.unwrap()["queue"]["length"], 1);

        let res = client
            .post(format!("{base}/stage/player/queue"))
            .header("Authorization", auth(&token))
            .json(&json!({ "action": "insert-next", "songId": 99 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["accepted"], true);
        assert_eq!(payload["action"], "insert-next");
        assert_eq!(payload["queue"]["length"], 1);
        assert_eq!(queues.lock().unwrap()[0]["songId"], 99);

        state.set_enabled(false, &store).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn external_play_routes_bridge_to_renderer() {
        let root = temp_root();
        let store = test_store(&root);
        let port = free_port();
        store
            .set(STAGE_API_PORT_KEY.to_string(), json!(port))
            .unwrap();

        let holder: Arc<Mutex<Option<StageState>>> = Arc::new(Mutex::new(None));
        let play_requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let holder_emit = holder.clone();
        let play_emit = play_requests.clone();
        let emit: EmitFn = Arc::new(move |channel, payload| {
            if channel == "stage-external-play-request" {
                play_emit.lock().unwrap().push(payload.clone());
                if let Some(state) = holder_emit.lock().unwrap().clone() {
                    let rid = payload["requestId"].as_str().unwrap().to_string();
                    let s = state.clone();
                    std::thread::spawn(move || {
                        let _ = s.complete_external_play(&json!({ "requestId": rid, "ok": true }));
                    });
                }
            }
        });
        let state = StageState::for_test(root.clone().join("stage"), emit, Arc::new(|| 0));
        *holder.lock().unwrap() = Some(state.clone());
        let status = enable_on_thread(state.clone(), &root);
        let token = status["token"].as_str().unwrap().to_string();
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;

        let res = client
            .post(format!("{base}/stage/play"))
            .header("Authorization", auth(&token))
            .json(&json!({ "songId": 123456 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["deprecated"], true);
        assert_eq!(payload["replacement"], "/stage/player/play");
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["songId"], 123456);
        assert_eq!(payload["appendToQueue"], false);
        assert_eq!(play_requests.lock().unwrap()[0]["songId"], 123456);

        // new player route with appendToQueue
        let res = client
            .post(format!("{base}/stage/player/play"))
            .header("Authorization", auth(&token))
            .json(&json!({ "songId": 654321, "appendToQueue": true }))
            .send()
            .await
            .unwrap();
        let payload: Value = res.json().await.unwrap();
        assert!(payload.get("deprecated").is_none());
        assert_eq!(payload["songId"], 654321);
        assert_eq!(payload["appendToQueue"], true);
        assert_eq!(play_requests.lock().unwrap()[1]["appendToQueue"], true);

        // invalid songId
        let res = client
            .post(format!("{base}/stage/player/play"))
            .header("Authorization", auth(&token))
            .json(&json!({ "songId": -1 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        state.set_enabled(false, &store).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn search_routes_normalize_results() {
        let root = temp_root();
        let store = test_store(&root);
        let port = free_port();
        store
            .set(STAGE_API_PORT_KEY.to_string(), json!(port))
            .unwrap();

        // Mock netease cloudsearch server (serves several requests).
        let mock = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let mock_port = mock.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let body = json!({
                "code": 200,
                "result": {
                    "songs": [{
                        "id": 42,
                        "name": "String Theocracy",
                        "ar": [{ "name": "Mili" }],
                        "al": { "name": "Library Of Ruina", "picUrl": "https://example.com/cover.jpg" },
                        "dt": 188000,
                    }]
                }
            })
            .to_string();
            loop {
                let Ok((socket, _)) = mock.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut reader = tokio::io::BufReader::new(socket);
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = reader.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let mut socket = reader.into_inner();
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.shutdown().await.ok();
                });
            }
        });

        let netease_port: PortFn = Arc::new(move || mock_port);
        let emit: EmitFn = Arc::new(|_, _| {});
        let state = StageState::for_test(root.clone().join("stage"), emit, netease_port);
        let status = enable_on_thread(state.clone(), &root);
        let token = status["token"].as_str().unwrap().to_string();
        let base = format!("http://127.0.0.1:{port}");
        let client = json_client().await;

        let res = client
            .post(format!("{base}/stage/player/search"))
            .header("Authorization", auth(&token))
            .json(&json!({ "query": "String Theocracy", "limit": 5 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["domain"], "player-playback");
        assert_eq!(payload["direction"], "outside-in");
        assert!(payload.get("deprecated").is_none());
        assert_eq!(payload["query"], "String Theocracy");
        assert_eq!(payload["songs"][0]["songId"], 42);
        assert_eq!(payload["songs"][0]["title"], "String Theocracy");
        assert_eq!(payload["songs"][0]["artists"], json!(["Mili"]));
        assert_eq!(payload["songs"][0]["album"], "Library Of Ruina");
        assert_eq!(
            payload["songs"][0]["coverUrl"],
            "https://example.com/cover.jpg"
        );
        assert_eq!(payload["songs"][0]["durationMs"], 188000);

        // deprecated route flags the replacement
        let res = client
            .post(format!("{base}/stage/search"))
            .header("Authorization", auth(&token))
            .json(&json!({ "query": "String Theocracy" }))
            .send()
            .await
            .unwrap();
        let payload: Value = res.json().await.unwrap();
        assert_eq!(payload["deprecated"], true);
        assert_eq!(payload["replacement"], "/stage/player/search");

        // empty query -> 400
        let res = client
            .post(format!("{base}/stage/player/search"))
            .header("Authorization", auth(&token))
            .json(&json!({ "query": "" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        state.set_enabled(false, &store).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn player_websocket_streams_events() {
        use tokio_stream::StreamExt as _;
        let context = test_context();
        let (token, port) = enable(&context);
        let (state, store, root) = &context;

        state.publish_player_snapshot(normal_snapshot(), None);

        let ws_url = format!("ws://127.0.0.1:{port}/stage/player/ws?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();

        // initial STATUS message
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let first: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(first["event"], "STATUS");
        assert_eq!(first["domain"], "player-playback");
        assert_eq!(first["direction"], "inside-out");
        assert!(first["queue"].get("items").is_none());

        // identical playback state does not emit PLAYBACK_UPDATED
        let mut noop = normal_snapshot();
        noop["positionMs"] = json!(2000);
        noop["durationMs"] = json!(30024);
        noop["sampledAtMs"] = json!(now_ms());
        state.publish_player_snapshot(noop, None);
        // no message expected: wait briefly
        let unexpected = tokio::time::timeout(Duration::from_millis(150), ws.next()).await;
        assert!(unexpected.is_err(), "expected silence after no-op publish");

        // forced playback event
        let mut forced = normal_snapshot();
        forced["positionMs"] = json!(5000);
        forced["durationMs"] = json!(30024);
        forced["sampledAtMs"] = json!(now_ms());
        state.publish_player_snapshot(forced, Some(json!({ "forcePlaybackEvent": true })));
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let play_msg: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(play_msg["event"], "PLAYBACK_UPDATED");
        assert_eq!(play_msg["playerState"], "PLAYING");
        assert!(play_msg.get("current").is_none());
        assert!(play_msg.get("queue").is_none());

        // track change -> TRACK_CHANGED
        state.publish_player_snapshot(
            json!({
                "playbackContext": "normal-playback",
                "current": {
                    "id": "43", "source": "netease", "title": "Next Track",
                    "artist": "Folia", "album": "Stage", "durationMs": 90000, "coverUrl": Value::Null,
                },
                "playerState": "PLAYING",
            }),
            None,
        );
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let track_msg: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(track_msg["event"], "TRACK_CHANGED");
        assert_eq!(track_msg["current"]["title"], "Next Track");
        assert!(track_msg.get("positionMs").is_none());

        // unauthenticated upgrade rejected
        let bad_url = format!("ws://127.0.0.1:{port}/stage/player/ws?token=wrong");
        let res = tokio_tungstenite::connect_async(&bad_url).await;
        assert!(res.is_err());

        state.set_enabled(false, store).unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn queue_revision_sha1_of_joined_ids() {
        let snapshot = normal_snapshot();
        assert_eq!(queue_key(Some(&snapshot)), "netease:42:0");
        let revision = queue_revision(Some(&snapshot));
        assert_eq!(revision.len(), 40);
        assert_eq!(revision, format!("{:x}", Sha1::digest(b"netease:42:0")));
        assert_eq!(queue_revision(None), format!("{:x}", Sha1::digest(b"")));
    }
}
