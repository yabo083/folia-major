//! M4 KuGou API bridge: Tauri commands `kugou_api_request` and `kugou_api_status`
//! that forward the ordinary search/audio/lyric operations to KuGou's real HTTP
//! endpoints using the same lite-client identity, signed-request scheme and
//! session/cookie handling as folia-major's `electron/kugouApiBridge.cjs` +
//! `node_modules/kugoumusicapi`.
//!
//! Contract:
//! - The operation allowlist is byte-identical to `kugouApiBridge.cjs`
//!   `OPERATION_MODULES` / `kugouTransport.ts` `KUGOU_OPERATIONS` (36 ops).
//!   Unknown operations are rejected with a structured error.
//! - Only the operations whose endpoint and parameter mapping is explicit in
//!   the reference are forwarded for real (`register_dev`, `search`, `audio`,
//!   `song_url`, `lyric`, `search_lyric`). Every other allowlisted operation
//!   (login_qr_*, user_*, playlist mutations, ...) returns an honest
//!   `unsupported` error.
//! - `kugou_api_status` reports `available: true` only after a real
//!   registration/request has succeeded; it never fabricates availability.
//! - Session cookies (`KUGOU_API_*` device identity + `dfid`/`token`/`userid`)
//!   live in managed state and are persisted to `<app_data_dir>/kugou_session.json`.
//! - Device-verification (errcode 20028 / "本次请求需要验证") triggers one forced
//!   re-registration + retry, mirroring `kugouApiBridge.cjs`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use aes::Aes128;
use base64::Engine as _;
use block_padding::Pkcs7;
use md5::Md5;
use rand::Rng;
use rsa::pkcs8::DecodePublicKey;
use rsa::Pkcs1v15Encrypt;
use rsa::RsaPublicKey;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::Digest;
use tauri::State;

// ---------------------------------------------------------------------------
// Reference-derived constants
// ---------------------------------------------------------------------------

/// Exact operation allowlist from `kugouApiBridge.cjs` / `kugouTransport.ts`.
pub const OPERATIONS: &[&str] = &[
    "register_dev",
    "login_qr_key",
    "login_qr_create",
    "login_qr_check",
    "logout",
    "user_detail",
    "user_vip_detail",
    "youth_union_vip",
    "youth_day_vip",
    "youth_day_vip_upgrade",
    "user_playlist",
    "user_cloud",
    "user_cloud_url",
    "search",
    "audio",
    "krm_audio",
    "song_url",
    "song_climax",
    "search_lyric",
    "lyric",
    "playlist_track_all",
    "playlist_detail",
    "album_detail",
    "album_songs",
    "artist_detail",
    "artist_albums",
    "artist_audios",
    "everyday_recommend",
    "everyday_history",
    "personal_fm",
    "top_card_youth",
    "playlist_add",
    "playlist_del",
    "playlist_tracks_add",
    "playlist_tracks_del",
];

/// Operations forwarded to KuGou's real HTTP endpoints. The other allowlisted
/// operations need login / signature machinery that this bridge does not
/// implement and are answered with an honest `unsupported` error.
const SUPPORTED_OPS: &[&str] = &[
    "register_dev",
    "search",
    "audio",
    "song_url",
    "lyric",
    "search_lyric",
];

const LITE_APPID: &str = "3116";
const LITE_CLIENTVER: &str = "11440";
const LITE_SIGN_KEY: &str = "LnT6xpN3khm36zse0QzvmgTZ3waWdRSA";
const LITE_KEY_SALT: &str = "185672dd44712f60bb1736df5a377e82";
const USER_AGENT: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";

const GATEWAY_BASE: &str = "https://gateway.kugou.com";
const USERSERVICE_BASE: &str = "https://userservice.kugou.com";
const LYRICS_BASE: &str = "https://lyrics.kugou.com";
const KMR_BASE: &str = "http://kmr.service.kugou.com";

const PUBLIC_LITE_RSA_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDECi0Np2UR87scwrvTr72L6oO0\n1rBbbBPriSDFPxr3Z5syug0O24QyQO8bg27+0+4kBzTBTBOZ/WWU0WryL1JSXRTX\nLgFVxtzIY41Pe7lPOgsfTCn5kZcvKhYKJesKnnJDNr5/abvTGf+rHG3YRwsCHcQ0\n8/q6ifSioBszvb3QiwIDAQAB\n-----END PUBLIC KEY-----";

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_PROXY: &str = "http://127.0.0.1:7890";
const SESSION_FILE: &str = "kugou_session.json";

const MAX_PARAMS_BYTES: usize = 32 * 1024;
const MAX_PARAMS_KEYS: usize = 32;

const RANDOM_ALPHABET: &[u8] = b"1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const KRC_KEY: [u8; 16] = [
    64, 71, 97, 119, 94, 50, 116, 71, 81, 54, 49, 45, 206, 210, 110, 105,
];

// ---------------------------------------------------------------------------
// Structured error
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KugouError {
    pub code: i64,
    pub kind: String,
    pub message: String,
}

impl KugouError {
    fn unknown_operation(operation: &str) -> Self {
        Self {
            code: 400,
            kind: "unknown_operation".to_string(),
            message: format!("Unsupported KuGou operation: {operation}"),
        }
    }

    fn invalid_params(message: &str) -> Self {
        Self {
            code: 400,
            kind: "invalid_params".to_string(),
            message: message.to_string(),
        }
    }

    fn unsupported(operation: &str) -> Self {
        Self {
            code: 501,
            kind: "unsupported".to_string(),
            message: format!(
                "KuGou operation `{operation}` is not implemented by this bridge; \
                 login/signature operations are unsupported"
            ),
        }
    }

    fn network(code: i64, message: String) -> Self {
        Self {
            code,
            kind: "network".to_string(),
            message,
        }
    }

    fn upstream(message: String) -> Self {
        Self {
            code: 502,
            kind: "upstream".to_string(),
            message,
        }
    }

    fn registration(message: &str) -> Self {
        Self {
            code: 502,
            kind: "registration".to_string(),
            message: message.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct KugouApiInner {
    client: reqwest::Client,
    cookies: Mutex<HashMap<String, String>>,
    available: Mutex<bool>,
    last_error: Mutex<Option<String>>,
    persist_path: Option<std::path::PathBuf>,
}

/// Managed by Tauri (`app.manage`); shared with the `kugou_api_request` and
/// `kugou_api_status` commands.
#[derive(Clone)]
pub struct KugouApiState {
    inner: Arc<KugouApiInner>,
}

impl KugouApiState {
    pub fn open(app_data_dir: impl AsRef<Path>) -> Self {
        let _ = std::fs::create_dir_all(app_data_dir.as_ref());
        let proxy = proxy_from_env().unwrap_or_else(|| DEFAULT_PROXY.to_string());
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(UPSTREAM_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT);
        if let Ok(proxy) = reqwest::Proxy::all(&proxy) {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().unwrap_or_else(|_| reqwest::Client::new());
        let persist_path = Some(app_data_dir.as_ref().join(SESSION_FILE));
        let cookies = load_cookies(persist_path.as_deref());
        let state = Self {
            inner: Arc::new(KugouApiInner {
                client,
                cookies: Mutex::new(cookies.clone()),
                available: Mutex::new(false),
                last_error: Mutex::new(None),
                persist_path,
            }),
        };
        state.persist_to_disk(&cookies);
        state
    }

    pub fn status_json(&self) -> Value {
        let available = *self.inner.available.lock().unwrap();
        let error = self.inner.last_error.lock().unwrap().clone();
        json!({ "available": available, "error": error })
    }

    pub async fn request(
        &self,
        operation: &str,
        params: Option<Value>,
    ) -> Result<Value, KugouError> {
        let operation = validate_operation(operation)?;
        let params = validate_params(params)?;
        if !SUPPORTED_OPS.contains(&operation) {
            return Err(KugouError::unsupported(operation));
        }
        if operation == "register_dev" {
            return self.run_register(false).await;
        }
        self.ensure_registered().await?;
        let mut body = self.forward_operation(operation, &params).await?;
        if is_device_verification_required(&body) {
            // Force re-registration (drops the stale dfid) then retry once.
            let _ = self.run_register(true).await;
            body = self.forward_operation(operation, &params).await?;
        }
        Ok(body)
    }

    async fn forward_operation(
        &self,
        operation: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, KugouError> {
        let cookies = self.snapshot_cookies();
        let spec = build_operation(operation, params, &cookies)?;
        let body = self.execute(spec).await?;
        if operation == "lyric"
            && params
                .get("decode")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            Ok(decode_lyric_body(body))
        } else {
            Ok(body)
        }
    }

    async fn ensure_registered(&self) -> Result<(), KugouError> {
        {
            let cookies = self.inner.cookies.lock().unwrap();
            if cookies.contains_key("dfid") {
                return Ok(());
            }
        }
        self.run_register(false).await.map(|_| ())
    }

    async fn run_register(&self, force: bool) -> Result<Value, KugouError> {
        {
            let mut cookies = self.inner.cookies.lock().unwrap();
            if !force && cookies.contains_key("dfid") {
                return Ok(
                    json!({ "status": 1, "data": { "dfid": cookies.get("dfid").cloned().unwrap_or_default() } }),
                );
            }
            if force {
                cookies.remove("dfid");
            }
        }
        self.persist_snapshot();

        let cookies = self.snapshot_cookies();
        let (spec, aes_key) = build_register(&cookies)?;
        let url = if spec.query.is_empty() {
            spec.url.clone()
        } else {
            format!("{}?{}", spec.url, build_query_string(&spec.query))
        };
        let mut headers = common_headers(&spec.dfid, &spec.mid, &spec.clienttime, spec.x_router);
        headers.push(("Cookie".to_string(), self.session_cookie_header()));

        let (bytes, set_cookies) = send_request(
            &self.inner.client,
            spec.method,
            &url,
            &headers,
            spec.body.as_deref(),
            spec.content_type,
        )
        .await?;

        // register_dev responses are AES-encrypted arraybuffers; decrypt with
        // the per-request key, falling back to plain JSON for error bodies.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let body = playlist_aes_decrypt(&b64, &aes_key)
            .or_else(|_| serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string()))
            .unwrap_or(Value::Null);

        let mut cookies = self.snapshot_cookies();
        merge_response_session(&mut cookies, &set_cookies, &body);
        self.save_cookies(cookies);

        if !self.snapshot_cookies().contains_key("dfid") {
            self.mark_failure("KuGou device registration did not return a dfid".to_string());
            return Err(KugouError::registration(
                "KuGou device registration did not return a dfid",
            ));
        }
        self.mark_success();
        Ok(body)
    }

    async fn execute(&self, spec: RequestSpec) -> Result<Value, KugouError> {
        let url = if spec.query.is_empty() {
            spec.url.clone()
        } else {
            format!("{}?{}", spec.url, build_query_string(&spec.query))
        };
        let mut headers = common_headers(&spec.dfid, &spec.mid, &spec.clienttime, spec.x_router);
        headers.push(("Cookie".to_string(), self.session_cookie_header()));
        let (bytes, set_cookies) = send_request(
            &self.inner.client,
            spec.method,
            &url,
            &headers,
            spec.body.as_deref(),
            spec.content_type,
        )
        .await?;

        let body = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(_) => {
                let snippet = String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(120)
                    .collect::<String>();
                self.mark_failure(format!("upstream returned non-JSON body: {snippet}"));
                return Err(KugouError::upstream(format!(
                    "upstream returned non-JSON body: {snippet}"
                )));
            }
        };

        let mut cookies = self.snapshot_cookies();
        merge_response_session(&mut cookies, &set_cookies, &body);
        self.save_cookies(cookies);

        if let Some(message) = upstream_error(&body) {
            self.mark_failure(message.clone());
            return Err(KugouError::upstream(message));
        }
        self.mark_success();
        Ok(body)
    }

    fn snapshot_cookies(&self) -> HashMap<String, String> {
        self.inner.cookies.lock().unwrap().clone()
    }

    fn session_cookie_header(&self) -> String {
        let cookies = self.snapshot_cookies();
        let mut pairs: Vec<(String, String)> = cookies.into_iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn save_cookies(&self, cookies: HashMap<String, String>) {
        *self.inner.cookies.lock().unwrap() = cookies.clone();
        self.persist_to_disk(&cookies);
    }

    fn persist_snapshot(&self) {
        let cookies = self.snapshot_cookies();
        self.persist_to_disk(&cookies);
    }

    fn persist_to_disk(&self, cookies: &HashMap<String, String>) {
        if let Some(path) = &self.inner.persist_path {
            if let Ok(text) = serde_json::to_string(cookies) {
                let _ = std::fs::write(path, text);
            }
        }
    }

    fn mark_success(&self) {
        *self.inner.available.lock().unwrap() = true;
        *self.inner.last_error.lock().unwrap() = None;
    }

    fn mark_failure(&self, message: String) {
        *self.inner.last_error.lock().unwrap() = Some(message);
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn kugou_api_request(
    state: State<'_, KugouApiState>,
    operation: String,
    params: Option<Value>,
) -> Result<Value, KugouError> {
    state.request(&operation, params).await
}

#[tauri::command]
pub fn kugou_api_status(state: State<'_, KugouApiState>) -> Value {
    state.status_json()
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_operation(operation: &str) -> Result<&'static str, KugouError> {
    OPERATIONS
        .iter()
        .find(|candidate| **candidate == operation)
        .copied()
        .ok_or_else(|| KugouError::unknown_operation(operation))
}

fn validate_params(params: Option<Value>) -> Result<Map<String, Value>, KugouError> {
    let map = match params {
        None | Some(Value::Null) => return Ok(Map::new()),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(KugouError::invalid_params("params must be a JSON object")),
    };
    if map.len() > MAX_PARAMS_KEYS {
        return Err(KugouError::invalid_params(&format!(
            "params exceed the maximum of {MAX_PARAMS_KEYS} keys"
        )));
    }
    if serde_json::to_vec(&map)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
        > MAX_PARAMS_BYTES
    {
        return Err(KugouError::invalid_params("params exceed the maximum size"));
    }
    for (key, value) in map.iter() {
        if !(value.is_string() || value.is_number() || value.is_boolean() || value.is_null()) {
            return Err(KugouError::invalid_params(&format!(
                "param `{key}` must be a scalar value"
            )));
        }
    }
    Ok(map)
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        Value::Null => None,
        Value::Array(_) | Value::Object(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Request building (pure, unit-testable)
// ---------------------------------------------------------------------------

struct RequestSpec {
    method: &'static str,
    url: String,
    query: Vec<(String, String)>,
    body: Option<String>,
    content_type: Option<&'static str>,
    x_router: Option<&'static str>,
    dfid: String,
    mid: String,
    clienttime: String,
}

fn build_operation(
    operation: &str,
    params: &Map<String, Value>,
    cookies: &HashMap<String, String>,
) -> Result<RequestSpec, KugouError> {
    match operation {
        "search" => Ok(build_search(params, cookies)),
        "audio" => Ok(build_audio(params, cookies)),
        "song_url" => Ok(build_song_url(params, cookies)),
        "lyric" => Ok(build_lyric(params, cookies)),
        "search_lyric" => Ok(build_search_lyric(params, cookies)),
        _ => Err(KugouError::unsupported(operation)),
    }
}

fn build_search(params: &Map<String, Value>, cookies: &HashMap<String, String>) -> RequestSpec {
    let (default, dfid, mid) = default_params(cookies, &dfid_of(cookies));
    let mut query: BTreeMap<String, String> = default.into_iter().collect();
    let raw_type = params
        .get("type")
        .and_then(scalar_string)
        .unwrap_or_else(|| "song".to_string());
    let search_type = if matches!(
        raw_type.as_str(),
        "special" | "lyric" | "song" | "album" | "author" | "mv"
    ) {
        raw_type
    } else {
        "song".to_string()
    };
    query.insert("albumhide".to_string(), "0".to_string());
    query.insert("iscorrection".to_string(), "1".to_string());
    let keyword = params
        .get("keywords")
        .and_then(scalar_string)
        .or_else(|| params.get("keyword").and_then(scalar_string))
        .unwrap_or_default();
    query.insert("keyword".to_string(), keyword);
    query.insert("nocollect".to_string(), "0".to_string());
    query.insert(
        "page".to_string(),
        params
            .get("page")
            .and_then(scalar_string)
            .unwrap_or_else(|| "1".to_string()),
    );
    query.insert(
        "pagesize".to_string(),
        params
            .get("pagesize")
            .and_then(scalar_string)
            .unwrap_or_else(|| "30".to_string()),
    );
    query.insert("platform".to_string(), "AndroidFilter".to_string());
    let signature = signature_android(&query, "");
    let mut pairs: Vec<(String, String)> = query.into_iter().collect();
    pairs.push(("signature".to_string(), signature));
    let url = if search_type == "song" {
        format!("{GATEWAY_BASE}/v3/search/song")
    } else {
        format!("{GATEWAY_BASE}/v1/search/{search_type}")
    };
    RequestSpec {
        method: "GET",
        url,
        query: pairs,
        body: None,
        content_type: None,
        x_router: Some("complexsearch.kugou.com"),
        dfid,
        mid,
        clienttime: now_sec().to_string(),
    }
}

fn build_audio(params: &Map<String, Value>, cookies: &HashMap<String, String>) -> RequestSpec {
    let dfid = dfid_of(cookies);
    let mid = mid_of(cookies);
    let date_time = now_ms();
    let hashes: Vec<String> = params
        .get("hash")
        .and_then(scalar_string)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let data_array = hashes
        .iter()
        .map(|h| format!("{{\"hash\":\"{}\",\"audio_id\":0}}", json_escape(h)))
        .collect::<Vec<_>>()
        .join(",");
    let data_array_json = format!("[{data_array}]");

    let key = sign_params_key(date_time);
    let mut body_parts = vec![
        format!("{{\"appid\":{LITE_APPID},\"clienttime\":{date_time},\"clientver\":{LITE_CLIENTVER},\"data\":{data_array_json},\"dfid\":\"{}\",\"key\":\"{key}\",\"mid\":\"{}\"", json_escape(&dfid), json_escape(&mid)),
    ];
    if let Some(token) = cookies.get("token") {
        if !token.is_empty() {
            body_parts.push(format!(",\"token\":\"{}\"", json_escape(token)));
        }
    }
    if let Some(userid) = cookies.get("userid") {
        if !userid.is_empty() && userid != "0" {
            body_parts.push(format!(",\"userid\":\"{}\"", json_escape(userid)));
        }
    }
    body_parts.push("}".to_string());
    let body = body_parts.concat();

    let mut sign_params: BTreeMap<String, String> =
        default_params(cookies, &dfid).0.into_iter().collect();
    sign_params.insert("clienttime".to_string(), date_time.to_string());
    sign_params.insert("data".to_string(), data_array_json);
    sign_params.insert("key".to_string(), key.clone());
    let signature = signature_android(&sign_params, &body);

    let mut query: Vec<(String, String)> = sign_params
        .iter()
        .filter(|(key, _)| key.as_str() != "data")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for hash in &hashes {
        query.push(("data[0].hash".to_string(), hash.clone()));
        query.push(("data[0].audio_id".to_string(), "0".to_string()));
    }
    query.push(("signature".to_string(), signature));

    RequestSpec {
        method: "POST",
        url: format!("{KMR_BASE}/v1/audio/audio"),
        query,
        body: Some(body),
        content_type: Some("application/json"),
        x_router: Some("kmr.service.kugou.com"),
        dfid,
        mid,
        clienttime: date_time.to_string(),
    }
}

fn build_song_url(params: &Map<String, Value>, cookies: &HashMap<String, String>) -> RequestSpec {
    let dfid = if cookies.contains_key("dfid") {
        cookies.get("dfid").cloned().unwrap_or_default()
    } else {
        random_string(24)
    };
    let mid = mid_of(cookies);
    let hash = params
        .get("hash")
        .and_then(scalar_string)
        .unwrap_or_default()
        .to_lowercase();
    let raw_quality = params.get("quality").and_then(scalar_string);
    let quality = match raw_quality.as_deref() {
        Some("piano") | Some("acappella") | Some("subwoofer") | Some("ancient") | Some("dj")
        | Some("surnay") => {
            format!("magic_{}", raw_quality.unwrap_or_default())
        }
        Some(value) => value.to_string(),
        None => "128".to_string(),
    };
    let free_part = params
        .get("free_part")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (default, _, _) = default_params(cookies, &dfid);
    let mut query: BTreeMap<String, String> = default.into_iter().collect();
    query.insert(
        "album_id".to_string(),
        params
            .get("album_id")
            .and_then(scalar_string)
            .unwrap_or_else(|| "0".to_string()),
    );
    query.insert("area_code".to_string(), "1".to_string());
    query.insert("hash".to_string(), hash.clone());
    query.insert("ssa_flag".to_string(), "is_fromtrack".to_string());
    query.insert("version".to_string(), "11436".to_string());
    query.insert("page_id".to_string(), "967177915".to_string());
    query.insert("quality".to_string(), quality);
    query.insert(
        "album_audio_id".to_string(),
        params
            .get("album_audio_id")
            .and_then(scalar_string)
            .unwrap_or_else(|| "0".to_string()),
    );
    query.insert("behavior".to_string(), "play".to_string());
    query.insert("pid".to_string(), "411".to_string());
    query.insert("cmd".to_string(), "26".to_string());
    query.insert("pidversion".to_string(), "3001".to_string());
    query.insert(
        "IsFreePart".to_string(),
        if free_part { "1" } else { "0" }.to_string(),
    );
    query.insert(
        "ppage_id".to_string(),
        "356753938,823673182,967485191".to_string(),
    );
    query.insert("cdnBackup".to_string(), "1".to_string());
    query.insert("kcard".to_string(), "0".to_string());
    query.insert("module".to_string(), String::new());
    let userid = cookies.get("userid").cloned().unwrap_or_default();
    let key = sign_key(&hash, &mid, &userid, LITE_APPID);
    query.insert("key".to_string(), key);
    let pairs: Vec<(String, String)> = query.into_iter().collect();

    RequestSpec {
        method: "GET",
        url: format!("{GATEWAY_BASE}/v5/url"),
        query: pairs,
        body: None,
        content_type: None,
        x_router: Some("trackercdn.kugou.com"),
        dfid,
        mid,
        clienttime: now_sec().to_string(),
    }
}

fn build_lyric(params: &Map<String, Value>, cookies: &HashMap<String, String>) -> RequestSpec {
    let (default, dfid, mid) = default_params(cookies, &dfid_of(cookies));
    let mut query: BTreeMap<String, String> = default.into_iter().collect();
    query.insert("ver".to_string(), "1".to_string());
    query.insert(
        "client".to_string(),
        params
            .get("client")
            .and_then(scalar_string)
            .unwrap_or_else(|| "android".to_string()),
    );
    query.insert(
        "id".to_string(),
        params.get("id").and_then(scalar_string).unwrap_or_default(),
    );
    query.insert(
        "accesskey".to_string(),
        params
            .get("accesskey")
            .and_then(scalar_string)
            .unwrap_or_default(),
    );
    query.insert(
        "fmt".to_string(),
        params
            .get("fmt")
            .and_then(scalar_string)
            .unwrap_or_else(|| "krc".to_string()),
    );
    query.insert("charset".to_string(), "utf8".to_string());
    let signature = signature_android(&query, "");
    let mut pairs: Vec<(String, String)> = query.into_iter().collect();
    pairs.push(("signature".to_string(), signature));
    RequestSpec {
        method: "GET",
        url: format!("{LYRICS_BASE}/download"),
        query: pairs,
        body: None,
        content_type: None,
        x_router: None,
        dfid,
        mid,
        clienttime: now_sec().to_string(),
    }
}

fn build_search_lyric(
    params: &Map<String, Value>,
    cookies: &HashMap<String, String>,
) -> RequestSpec {
    let mut query: Vec<(String, String)> = vec![
        (
            "album_audio_id".to_string(),
            params
                .get("album_audio_id")
                .and_then(scalar_string)
                .unwrap_or_else(|| "0".to_string()),
        ),
        ("appid".to_string(), LITE_APPID.to_string()),
        ("clientver".to_string(), LITE_CLIENTVER.to_string()),
        (
            "duration".to_string(),
            params
                .get("duration")
                .and_then(scalar_string)
                .unwrap_or_else(|| "0".to_string()),
        ),
        (
            "hash".to_string(),
            params
                .get("hash")
                .and_then(scalar_string)
                .unwrap_or_default(),
        ),
        (
            "keyword".to_string(),
            params
                .get("keywords")
                .and_then(scalar_string)
                .unwrap_or_default(),
        ),
        ("lrctxt".to_string(), "1".to_string()),
        (
            "man".to_string(),
            params
                .get("man")
                .and_then(scalar_string)
                .unwrap_or_else(|| "no".to_string()),
        ),
    ];
    query.sort_by(|a, b| a.0.cmp(&b.0));
    let dfid = dfid_of(cookies);
    let mid = mid_of(cookies);
    RequestSpec {
        method: "GET",
        url: format!("{LYRICS_BASE}/v1/search"),
        query,
        body: None,
        content_type: None,
        x_router: None,
        dfid,
        mid,
        clienttime: now_sec().to_string(),
    }
}

/// register_dev: RSA-encrypted `p` + AES-encrypted device body, signed with the
/// android signature over the merged params + body (mirrors `register_dev.js`).
fn build_register(cookies: &HashMap<String, String>) -> Result<(RequestSpec, String), KugouError> {
    let guid = cookies.get("KUGOU_API_GUID").cloned().unwrap_or_default();
    let userid = cookies.get("userid").cloned().unwrap_or_default();
    let token = cookies.get("token").cloned().unwrap_or_default();

    let device = build_device_info(&guid);
    let (aes_key, aes_str) = playlist_aes_encrypt(&device);
    let p_json = json!({
        "aes": aes_key,
        "uid": if userid.is_empty() { json!(0) } else { json!(userid) },
        "token": token,
    });
    let p = rsa_encrypt2_json(&p_json)?;

    let (default, dfid, mid) = default_params(cookies, &dfid_of(cookies));
    let mut sign_params: BTreeMap<String, String> = default.into_iter().collect();
    sign_params.insert("part".to_string(), "1".to_string());
    sign_params.insert("platid".to_string(), "1".to_string());
    sign_params.insert("p".to_string(), p);
    let signature = signature_android(&sign_params, &aes_str);
    let mut query: Vec<(String, String)> = sign_params.into_iter().collect();
    query.push(("signature".to_string(), signature));
    let clienttime = now_sec().to_string();

    Ok((
        RequestSpec {
            method: "POST",
            url: format!("{USERSERVICE_BASE}/risk/v2/r_register_dev"),
            query,
            body: Some(aes_str),
            content_type: Some("text/plain;charset=utf-8"),
            x_router: None,
            dfid,
            mid,
            clienttime,
        },
        aes_key,
    ))
}

fn build_device_info(guid: &str) -> Value {
    json!({
        "availableRamSize": 4983533568_i64,
        "availableRomSize": 48114719,
        "availableSDSize": 48114717,
        "basebandVer": "",
        "batteryLevel": 100,
        "batteryStatus": 3,
        "brand": "Redmi",
        "buildSerial": "unknown",
        "device": "marble",
        "imei": guid,
        "imsi": "",
        "manufacturer": "Xiaomi",
        "uuid": guid,
        "accelerometer": false,
        "accelerometerValue": "",
        "gravity": false,
        "gravityValue": "",
        "gyroscope": false,
        "gyroscopeValue": "",
        "light": false,
        "lightValue": "",
        "magnetic": false,
        "magneticValue": "",
        "orientation": false,
        "orientationValue": "",
        "pressure": false,
        "pressureValue": "",
        "step_counter": false,
        "step_counterValue": "",
        "temperature": false,
        "temperatureValue": "",
    })
}

// ---------------------------------------------------------------------------
// HTTP forwarding
// ---------------------------------------------------------------------------

async fn send_request(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    content_type: Option<&str>,
) -> Result<(Vec<u8>, Vec<String>), KugouError> {
    let mut request = if method == "GET" {
        client.get(url)
    } else {
        client.post(url)
    };
    for (key, value) in headers {
        request = request.header(key, value);
    }
    if let Some(content_type) = content_type {
        request = request.header("Content-Type", content_type);
    }
    if let Some(body) = body {
        request = request.body(body.to_string());
    }
    let response = request.send().await.map_err(|e| {
        let code = if e.is_timeout() { 504 } else { 502 };
        KugouError::network(code, format!("upstream request failed: {e}"))
    })?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        return Err(KugouError::upstream(format!(
            "upstream returned HTTP {status}"
        )));
    }
    let set_cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap_or_default().to_string())
        .collect();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| KugouError::network(502, format!("upstream body read failed: {e}")))?;
    Ok((bytes.to_vec(), set_cookies))
}

fn common_headers(
    dfid: &str,
    mid: &str,
    clienttime: &str,
    x_router: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("dfid".to_string(), dfid.to_string()),
        ("clienttime".to_string(), clienttime.to_string()),
        ("mid".to_string(), mid.to_string()),
        ("kg-rc".to_string(), "1".to_string()),
        ("kg-thash".to_string(), "5d816a0".to_string()),
        ("kg-rec".to_string(), "1".to_string()),
        (
            "kg-rf".to_string(),
            "B9EDA08A64250DEFFBCADDEE00F8F25F".to_string(),
        ),
    ];
    if let Some(router) = x_router {
        headers.push(("x-router".to_string(), router.to_string()));
    }
    headers
}

fn proxy_from_env() -> Option<String> {
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
    ] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Session / cookie handling (mirrors kugouApiBridge.cjs)
// ---------------------------------------------------------------------------

fn create_device_cookies() -> HashMap<String, String> {
    let mut rng = rand::thread_rng();
    let guid_bytes: [u8; 16] = rng.gen();
    let guid = hex::encode(guid_bytes).to_uppercase();
    let digest = md5_hex(guid.as_bytes());
    let mid = big_hex_to_decimal(&digest).unwrap_or_default();
    let mac: Vec<String> = (0..6).map(|_| format!("{:02X}", rng.gen::<u8>())).collect();
    let webgl = rng.gen::<u64>().to_string();
    let mut cookies = HashMap::new();
    cookies.insert("KUGOU_API_PLATFORM".to_string(), "lite".to_string());
    cookies.insert("KUGOU_API_GUID".to_string(), guid);
    cookies.insert("KUGOU_API_MID".to_string(), mid);
    cookies.insert("KUGOU_API_DEV".to_string(), random_upper_hex(5));
    cookies.insert("KUGOU_API_MAC".to_string(), mac.join(":"));
    cookies.insert("KUGOU_API_WEBGL".to_string(), webgl);
    cookies
}

fn load_cookies(path: Option<&Path>) -> HashMap<String, String> {
    if let Some(path) = path {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
                let mut cookies = HashMap::new();
                for (key, value) in map {
                    if let Value::String(value) = value {
                        cookies.insert(key, value);
                    }
                }
                if !cookies.is_empty() {
                    return cookies;
                }
            }
        }
    }
    create_device_cookies()
}

/// Mirrors `parseCookieEntry` in kugouApiBridge.cjs.
fn parse_cookie_entry(entry: &str) -> Option<(String, String)> {
    let first_part = entry.split(';').next().unwrap_or_default();
    let separator = first_part.find('=')?;
    if separator == 0 {
        return None;
    }
    Some((
        first_part[..separator].trim().to_string(),
        first_part[separator + 1..].trim().to_string(),
    ))
}

/// Mirrors `mergeResponseSession` in kugouApiBridge.cjs: fold Set-Cookie
/// headers plus `token` / `userid` / `dfid` from the response body into the
/// persisted session.
fn merge_response_session(
    cookies: &mut HashMap<String, String>,
    set_cookies: &[String],
    body: &Value,
) {
    for entry in set_cookies {
        if let Some((key, value)) = parse_cookie_entry(entry) {
            cookies.insert(key, value);
        }
    }
    let data = body
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(body);
    if let Some(token) = data.get("token").and_then(Value::as_str) {
        cookies.insert("token".to_string(), token.to_string());
    }
    if let Some(userid) = data
        .get("userid")
        .or_else(|| data.get("user_id"))
        .and_then(Value::as_str)
    {
        cookies.insert("userid".to_string(), userid.to_string());
    }
    if let Some(dfid) = data.get("dfid").and_then(Value::as_str) {
        cookies.insert("dfid".to_string(), dfid.to_string());
    }
}

/// Mirrors `isDeviceVerificationRequired` in the reference.
fn is_device_verification_required(body: &Value) -> bool {
    let error_code = body
        .get("errcode")
        .or_else(|| body.get("error_code"))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        });
    let message = body
        .get("error")
        .or_else(|| body.get("error_msg"))
        .or_else(|| body.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("");
    error_code == Some(20028) || message.contains("本次请求需要验证")
}

/// Mirrors the `createRequest` status gate: reject when the upstream body has
/// `status === 0` or a truthy non-zero `error_code`.
fn upstream_error(body: &Value) -> Option<String> {
    if let Some(status) = body.get("status").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    }) {
        if status == 0 {
            return Some(message_of(body, "upstream returned status 0"));
        }
    }
    if let Some(error_code) = body.get("error_code").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    }) {
        if error_code != 0 {
            return Some(message_of(
                body,
                &format!("upstream returned error_code {error_code}"),
            ));
        }
    }
    None
}

fn message_of(body: &Value, fallback: &str) -> String {
    for key in ["msg", "message", "error_msg", "error"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return format!("{fallback}: {value}");
            }
        }
    }
    fallback.to_string()
}

// ---------------------------------------------------------------------------
// Lyric decoding (mirrors lyric.js + util.js decodeLyrics)
// ---------------------------------------------------------------------------

fn decode_lyric_body(mut body: Value) -> Value {
    let Some(content) = body.get("content").and_then(Value::as_str) else {
        return body;
    };
    if content.is_empty() {
        return body;
    }
    let fmt = body.get("fmt").and_then(Value::as_str).unwrap_or("krc");
    let content_type = body.get("contenttype").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    });
    let decoded = if fmt == "lrc" || content_type.unwrap_or(0) != 0 {
        base64::engine::general_purpose::STANDARD
            .decode(content)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    } else {
        krc_decode(content)
    };
    if let Some(text) = decoded {
        body["decodeContent"] = json!(text);
    }
    body
}

fn krc_decode(content: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content)
        .ok()?;
    if bytes.len() < 4 {
        return None;
    }
    let mut payload: Vec<u8> = bytes[4..].to_vec();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= KRC_KEY[index % KRC_KEY.len()];
    }
    let mut decoder = flate2::read::ZlibDecoder::new(payload.as_slice());
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut out).ok()?;
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------
// Crypto helpers (ported from kugoumusicapi/util/crypto.js)
// ---------------------------------------------------------------------------

fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data))
}

fn aes_cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let key = GenericArray::clone_from_slice(key);
    let iv = GenericArray::clone_from_slice(iv);
    cbc::Encryptor::<Aes128>::new(&key, &iv).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

fn aes_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let key = GenericArray::clone_from_slice(key);
    let iv = GenericArray::clone_from_slice(iv);
    cbc::Decryptor::<Aes128>::new(&key, &iv)
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|e| format!("AES-CBC unpad failed: {e}"))
}

/// `playlistAesEncrypt`: AES-128-CBC keyed by `md5(key)[0..16]`/`md5(key)[16..32]`
/// (ASCII hex strings used directly as key bytes), base64 output.
fn playlist_aes_encrypt(data: &Value) -> (String, String) {
    let key = random_string(6).to_lowercase();
    let digest = md5_hex(key.as_bytes());
    let plaintext = serde_json::to_string(data).unwrap_or_default();
    let ciphertext = aes_cbc_encrypt(
        digest[..16].as_bytes(),
        digest[16..32].as_bytes(),
        plaintext.as_bytes(),
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(ciphertext);
    (key, encoded)
}

/// `playlistAesDecrypt`: inverse of `playlistAesEncrypt`.
fn playlist_aes_decrypt(str_base64: &str, key: &str) -> Result<Value, String> {
    let digest = md5_hex(key.as_bytes());
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(str_base64)
        .map_err(|e| e.to_string())?;
    let plaintext = aes_cbc_decrypt(
        digest[..16].as_bytes(),
        digest[16..32].as_bytes(),
        &ciphertext,
    )?;
    let text = String::from_utf8(plaintext).map_err(|e| e.to_string())?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => Ok(value),
        Err(_) => Ok(Value::String(text)),
    }
}

/// `rsaEncrypt2`: PKCS#1 v1.5 RSA encryption (lite public key), hex output.
fn rsa_encrypt2_json(value: &Value) -> Result<String, KugouError> {
    let public_key = RsaPublicKey::from_public_key_pem(PUBLIC_LITE_RSA_PEM)
        .map_err(|e| KugouError::network(500, format!("invalid RSA public key: {e}")))?;
    let data = serde_json::to_vec(value).unwrap_or_default();
    let ciphertext = public_key
        .encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, &data)
        .map_err(|e| KugouError::network(500, format!("RSA encrypt failed: {e}")))?;
    Ok(hex::encode(ciphertext))
}

// ---------------------------------------------------------------------------
// Signature helpers (ported from kugoumusicapi/util/helper.js)
// ---------------------------------------------------------------------------

/// `signatureAndroidParams` (lite): `md5(key + sorted(k=v...) + data + key)`.
fn signature_android(params: &BTreeMap<String, String>, data: &str) -> String {
    let mut entries: Vec<(&String, &String)> = params.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut source = String::new();
    for (key, value) in entries {
        source.push_str(&format!("{key}={value}"));
    }
    md5_hex(format!("{LITE_SIGN_KEY}{source}{data}{LITE_SIGN_KEY}").as_bytes())
}

/// `signKey` (lite): `md5(hash + salt + appid + mid + userid)`.
fn sign_key(hash: &str, mid: &str, userid: &str, appid: &str) -> String {
    let userid = if userid.is_empty() { "0" } else { userid };
    md5_hex(format!("{hash}{LITE_KEY_SALT}{appid}{mid}{userid}").as_bytes())
}

/// `signParamsKey` (lite): `md5(appid + salt + clientver + data)`.
fn sign_params_key(data: i64) -> String {
    md5_hex(format!("{LITE_APPID}{LITE_SIGN_KEY}{LITE_CLIENTVER}{data}").as_bytes())
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn dfid_of(cookies: &HashMap<String, String>) -> String {
    cookies
        .get("dfid")
        .cloned()
        .unwrap_or_else(|| "-".to_string())
}

fn mid_of(cookies: &HashMap<String, String>) -> String {
    cookies.get("KUGOU_API_MID").cloned().unwrap_or_default()
}

fn default_params(
    cookies: &HashMap<String, String>,
    dfid: &str,
) -> (Vec<(String, String)>, String, String) {
    let mid = mid_of(cookies);
    let clienttime = now_sec();
    let mut params = vec![
        ("dfid".to_string(), dfid.to_string()),
        ("mid".to_string(), mid.clone()),
        ("uuid".to_string(), "-".to_string()),
        ("appid".to_string(), LITE_APPID.to_string()),
        ("clientver".to_string(), LITE_CLIENTVER.to_string()),
        ("clienttime".to_string(), clienttime.to_string()),
    ];
    if let Some(token) = cookies.get("token") {
        if !token.is_empty() {
            params.push(("token".to_string(), token.clone()));
        }
    }
    if let Some(userid) = cookies.get("userid") {
        if !userid.is_empty() && userid != "0" {
            params.push(("userid".to_string(), userid.clone()));
        }
    }
    (params, dfid.to_string(), mid)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_sec() -> i64 {
    now_ms() / 1000
}

fn big_hex_to_decimal(hex_str: &str) -> Option<String> {
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() > 16 {
        return None;
    }
    let mut buffer = [0u8; 16];
    buffer[16 - bytes.len()..].copy_from_slice(&bytes);
    Some(u128::from_be_bytes(buffer).to_string())
}

fn random_string(len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| RANDOM_ALPHABET[rng.gen_range(0..RANDOM_ALPHABET.len())] as char)
        .collect()
}

fn random_upper_hex(len: usize) -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
    hex::encode(bytes).to_uppercase()
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
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

fn build_query_string(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                uri_component_encode(key),
                uri_component_encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cookies_with(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut cookies = create_device_cookies();
        for (key, value) in extra {
            cookies.insert(key.to_string(), value.to_string());
        }
        cookies
    }

    #[test]
    fn allowlist_matches_reference_exactly() {
        let reference = [
            "register_dev",
            "login_qr_key",
            "login_qr_create",
            "login_qr_check",
            "logout",
            "user_detail",
            "user_vip_detail",
            "youth_union_vip",
            "youth_day_vip",
            "youth_day_vip_upgrade",
            "user_playlist",
            "user_cloud",
            "user_cloud_url",
            "search",
            "audio",
            "krm_audio",
            "song_url",
            "song_climax",
            "search_lyric",
            "lyric",
            "playlist_track_all",
            "playlist_detail",
            "album_detail",
            "album_songs",
            "artist_detail",
            "artist_albums",
            "artist_audios",
            "everyday_recommend",
            "everyday_history",
            "personal_fm",
            "top_card_youth",
            "playlist_add",
            "playlist_del",
            "playlist_tracks_add",
            "playlist_tracks_del",
        ];
        assert_eq!(OPERATIONS.len(), reference.len());
        assert_eq!(OPERATIONS, reference);
    }

    #[test]
    fn allowlist_rejects_unknown_operation() {
        let err = validate_operation("totally_made_up").unwrap_err();
        assert_eq!(err.kind, "unknown_operation");
        assert_eq!(err.code, 400);
        assert!(err.message.contains("totally_made_up"));
    }

    #[test]
    fn supported_ops_are_a_subset_of_allowlist() {
        for operation in SUPPORTED_OPS {
            assert!(
                OPERATIONS.contains(operation),
                "{operation} not in allowlist"
            );
        }
    }

    #[test]
    fn unsupported_allowlisted_operations_reject_honestly() {
        for operation in [
            "login_qr_key",
            "login_qr_create",
            "login_qr_check",
            "logout",
            "user_detail",
            "user_playlist",
            "playlist_add",
            "krm_audio",
            "song_climax",
        ] {
            let err = KugouError::unsupported(operation);
            assert_eq!(err.kind, "unsupported");
            assert_eq!(err.code, 501);
            assert!(err.message.contains(operation));
            assert!(err.message.contains("unsupported"));
        }
    }

    #[test]
    fn params_accept_bounded_object_and_reject_invalid_shapes() {
        let valid = validate_params(Some(json!({ "keywords": "hello", "page": 2 }))).unwrap();
        assert_eq!(valid.get("keywords").unwrap(), &json!("hello"));

        for invalid in [json!("x"), json!([1, 2]), json!(42), json!(true)] {
            let err = validate_params(Some(invalid)).unwrap_err();
            assert_eq!(err.kind, "invalid_params");
        }
        assert!(validate_params(None).unwrap().is_empty());
        assert!(validate_params(Some(Value::Null)).unwrap().is_empty());
    }

    #[test]
    fn params_reject_oversized_and_too_many_keys() {
        let mut big = Map::new();
        for i in 0..(MAX_PARAMS_KEYS + 1) {
            big.insert(format!("key{i}"), json!("value"));
        }
        let err = validate_params(Some(Value::Object(big))).unwrap_err();
        assert_eq!(err.kind, "invalid_params");

        let huge_value = "x".repeat(MAX_PARAMS_BYTES + 1);
        let err = validate_params(Some(json!({ "keywords": huge_value }))).unwrap_err();
        assert_eq!(err.kind, "invalid_params");
    }

    #[test]
    fn params_reject_nested_values() {
        let err = validate_params(Some(json!({ "nested": { "a": 1 } }))).unwrap_err();
        assert_eq!(err.kind, "invalid_params");
        let err = validate_params(Some(json!({ "list": [1, 2] }))).unwrap_err();
        assert_eq!(err.kind, "invalid_params");
    }

    #[test]
    fn cookie_entry_parses_first_segment_only() {
        assert_eq!(
            parse_cookie_entry("token=abc; Path=/; HttpOnly")
                .as_ref()
                .map(|(k, v)| (k.as_str(), v.as_str())),
            Some(("token", "abc"))
        );
        assert_eq!(
            parse_cookie_entry("dfid=xyz")
                .as_ref()
                .map(|(k, v)| (k.as_str(), v.as_str())),
            Some(("dfid", "xyz"))
        );
        assert!(parse_cookie_entry("=novalue").is_none());
        assert!(parse_cookie_entry("").is_none());
    }

    #[test]
    fn merge_response_session_merges_set_cookie_and_body() {
        let mut cookies = cookies_with(&[("dfid", "old")]);
        let body =
            json!({ "status": 1, "data": { "token": "t1", "userid": "u1", "dfid": "new-dfid" } });
        merge_response_session(
            &mut cookies,
            &[
                "token=from-set-cookie; Path=/".to_string(),
                "other=1; HttpOnly".to_string(),
            ],
            &body,
        );
        // set-cookie headers are folded in first; the response body's token /
        // userid / dfid then take precedence (mirrors mergeResponseSession).
        assert_eq!(cookies.get("token").map(String::as_str), Some("t1"));
        assert_eq!(cookies.get("userid").map(String::as_str), Some("u1"));
        assert_eq!(cookies.get("dfid").map(String::as_str), Some("new-dfid"));
        assert_eq!(cookies.get("other").map(String::as_str), Some("1"));
    }

    #[test]
    fn merge_response_session_reads_body_data_variants() {
        let mut cookies = HashMap::new();
        merge_response_session(
            &mut cookies,
            &[],
            &json!({ "data": { "user_id": "999", "token": "tok" } }),
        );
        assert_eq!(cookies.get("userid").map(String::as_str), Some("999"));
        assert_eq!(cookies.get("token").map(String::as_str), Some("tok"));

        let mut cookies = HashMap::new();
        merge_response_session(&mut cookies, &[], &json!({ "token": "top", "dfid": "d" }));
        assert_eq!(cookies.get("token").map(String::as_str), Some("top"));
        assert_eq!(cookies.get("dfid").map(String::as_str), Some("d"));
    }

    #[test]
    fn error_mapping_detects_upstream_status_zero() {
        assert!(upstream_error(&json!({ "status": 0, "msg": "boom" }))
            .unwrap()
            .contains("boom"));
        assert!(upstream_error(&json!({ "error_code": 1 })).is_some());
        assert!(upstream_error(&json!({ "error_code": "1" })).is_some());
        assert!(upstream_error(&json!({ "status": 1 })).is_none());
        assert!(upstream_error(&json!({ "status": "1" })).is_none());
        assert!(upstream_error(&json!({ "error_code": 0 })).is_none());
        assert!(upstream_error(&json!({})).is_none());
    }

    #[test]
    fn error_mapping_detects_device_verification() {
        assert!(is_device_verification_required(
            &json!({ "errcode": 20028 })
        ));
        assert!(is_device_verification_required(
            &json!({ "error_code": "20028" })
        ));
        assert!(is_device_verification_required(
            &json!({ "msg": "本次请求需要验证" })
        ));
        assert!(!is_device_verification_required(&json!({ "errcode": 200 })));
        assert!(!is_device_verification_required(&json!({ "msg": "ok" })));
    }

    #[test]
    fn signature_android_format_is_stable() {
        let mut params = BTreeMap::new();
        params.insert("appid".to_string(), "3116".to_string());
        params.insert("clienttime".to_string(), "1000".to_string());
        params.insert("dfid".to_string(), "-".to_string());
        let signature = signature_android(&params, "");
        let expected = md5_hex(
            format!("{LITE_SIGN_KEY}appid=3116clienttime=1000dfid=-{LITE_SIGN_KEY}").as_bytes(),
        );
        assert_eq!(signature, expected);
        assert_eq!(signature.len(), 32);
    }

    #[test]
    fn sign_key_matches_reference_layout() {
        let key = sign_key("abc", "mid123", "", "3116");
        let expected = md5_hex(format!("abc{LITE_KEY_SALT}3116mid1230").as_bytes());
        assert_eq!(key, expected);
    }

    #[test]
    fn sign_params_key_matches_reference_layout() {
        let key = sign_params_key(1234567890);
        let expected =
            md5_hex(format!("{LITE_APPID}{LITE_SIGN_KEY}{LITE_CLIENTVER}1234567890").as_bytes());
        assert_eq!(key, expected);
    }

    #[test]
    fn playlist_aes_roundtrip() {
        let data = json!({ "device": "marble", "uuid": "ABC123" });
        let (key, encrypted) = playlist_aes_encrypt(&data);
        assert!(!key.is_empty() && key.len() <= 6);
        let decrypted = playlist_aes_decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn rsa_encrypt2_returns_hex() {
        let ciphertext =
            rsa_encrypt2_json(&json!({ "aes": "abc123", "uid": 0, "token": "" })).unwrap();
        assert_eq!(ciphertext.len(), 256);
        assert!(ciphertext.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn krc_decode_roundtrip() {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let plaintext = b"<krc><content>hello</content></krc>";
        let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(plaintext).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut payload = vec![0u8, 0, 0, 4];
        payload.extend_from_slice(&compressed);
        for (index, byte) in payload[4..].iter_mut().enumerate() {
            *byte ^= KRC_KEY[index % KRC_KEY.len()];
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&payload);
        assert_eq!(
            krc_decode(&encoded).as_deref(),
            Some("<krc><content>hello</content></krc>")
        );
    }

    #[test]
    fn device_cookies_have_expected_shape() {
        let cookies = create_device_cookies();
        assert_eq!(
            cookies.get("KUGOU_API_PLATFORM").map(String::as_str),
            Some("lite")
        );
        assert_eq!(cookies.get("KUGOU_API_GUID").map(|s| s.len()), Some(32));
        let mid = cookies.get("KUGOU_API_MID").unwrap();
        assert!(mid.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(cookies.get("KUGOU_API_DEV").map(|s| s.len()), Some(10));
        assert!(cookies.get("KUGOU_API_MAC").unwrap().contains(':'));
        assert!(cookies
            .get("KUGOU_API_WEBGL")
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn big_hex_to_decimal_matches_bigint() {
        // md5("kugou") -> 128-bit value; assert decimal form parses back to same hex.
        let hex_str = md5_hex(b"kugou");
        let decimal = big_hex_to_decimal(&hex_str).unwrap();
        let back = format!("{:032x}", u128::from_str_radix(&decimal, 10).unwrap());
        assert_eq!(back, hex_str);
    }

    #[test]
    fn search_builds_expected_query() {
        let mut params = Map::new();
        params.insert("keywords".to_string(), json!("周杰伦"));
        params.insert("page".to_string(), json!(1));
        params.insert("pagesize".to_string(), json!(30));
        let cookies = cookies_with(&[("dfid", "some-dfid")]);
        let spec = build_search(&params, &cookies);
        assert_eq!(spec.url, format!("{GATEWAY_BASE}/v3/search/song"));
        assert_eq!(spec.method, "GET");
        assert_eq!(spec.x_router, Some("complexsearch.kugou.com"));
        let query: HashMap<String, String> = spec.query.clone().into_iter().collect();
        assert_eq!(query.get("keyword").map(String::as_str), Some("周杰伦"));
        assert_eq!(query.get("page").map(String::as_str), Some("1"));
        assert_eq!(
            query.get("platform").map(String::as_str),
            Some("AndroidFilter")
        );
        assert!(query.get("signature").map(|s| s.len()) == Some(32));
        assert_eq!(spec.dfid, "some-dfid");
    }

    #[test]
    fn search_supports_non_song_types() {
        let mut params = Map::new();
        params.insert("keywords".to_string(), json!("x"));
        params.insert("type".to_string(), json!("album"));
        let cookies = create_device_cookies();
        let spec = build_search(&params, &cookies);
        assert_eq!(spec.url, format!("{GATEWAY_BASE}/v1/search/album"));
        let mut params = Map::new();
        params.insert("keywords".to_string(), json!("x"));
        params.insert("type".to_string(), json!("bogus"));
        let spec = build_search(&params, &cookies);
        assert_eq!(spec.url, format!("{GATEWAY_BASE}/v3/search/song"));
    }

    #[test]
    fn audio_builds_signed_json_body_and_bracket_query() {
        let mut params = Map::new();
        params.insert("hash".to_string(), json!("HASH001"));
        let cookies = cookies_with(&[
            ("dfid", "d1"),
            ("KUGOU_API_MID", "900000000000000000000000000000000000001"),
        ]);
        let spec = build_audio(&params, &cookies);
        assert_eq!(spec.url, format!("{KMR_BASE}/v1/audio/audio"));
        assert_eq!(spec.method, "POST");
        assert_eq!(spec.content_type, Some("application/json"));
        let body = spec.body.clone().unwrap();
        assert!(body.contains("\"data\":[{\"hash\":\"HASH001\",\"audio_id\":0}]"));
        assert!(body.contains("\"appid\":3116"));
        assert!(body.contains("\"dfid\":\"d1\""));
        assert!(body.contains("\"key\":"));
        // The signature must cover the exact body string and the data array.
        let query: HashMap<String, String> = spec.query.clone().into_iter().collect();
        assert_eq!(
            query.get("data[0].hash").map(String::as_str),
            Some("HASH001")
        );
        assert_eq!(query.get("data[0].audio_id").map(String::as_str), Some("0"));
        assert!(query.get("signature").map(|s| s.len()) == Some(32));
        let query_len = query.len();
        let mut sign_params: BTreeMap<String, String> = query
            .clone()
            .into_iter()
            .filter(|(key, _)| !key.starts_with("data[") && key != "signature")
            .collect();
        sign_params.insert(
            "data".to_string(),
            "[{\"hash\":\"HASH001\",\"audio_id\":0}]".to_string(),
        );
        let expected = signature_android(&sign_params, &body);
        // The signature that build_audio shipped must equal what the server
        // recomputes from the bracketed query + the exact JSON body.
        assert_eq!(
            query.get("signature").map(String::as_str),
            Some(expected.as_str())
        );
        assert_eq!(query_len, spec.query.len());
        assert_eq!(expected.len(), 32);
    }

    #[test]
    fn song_url_builds_key_without_signature() {
        let mut params = Map::new();
        params.insert("hash".to_string(), json!("abcdef"));
        params.insert("quality".to_string(), json!("128"));
        let cookies = cookies_with(&[("dfid", "d1")]);
        let spec = build_song_url(&params, &cookies);
        assert_eq!(spec.url, format!("{GATEWAY_BASE}/v5/url"));
        assert_eq!(spec.x_router, Some("trackercdn.kugou.com"));
        let query: HashMap<String, String> = spec.query.clone().into_iter().collect();
        assert_eq!(query.get("hash").map(String::as_str), Some("abcdef"));
        assert_eq!(query.get("quality").map(String::as_str), Some("128"));
        assert_eq!(query.get("pid").map(String::as_str), Some("411"));
        assert_eq!(query.get("page_id").map(String::as_str), Some("967177915"));
        assert!(query.contains_key("key"));
        assert!(!query.contains_key("signature"));
    }

    #[test]
    fn song_url_lowercases_hash_and_maps_magic_quality() {
        let mut params = Map::new();
        params.insert("hash".to_string(), json!("ABC123"));
        params.insert("quality".to_string(), json!("acappella"));
        let cookies = create_device_cookies();
        let spec = build_song_url(&params, &cookies);
        let query: HashMap<String, String> = spec.query.into_iter().collect();
        assert_eq!(query.get("hash").map(String::as_str), Some("abc123"));
        assert_eq!(
            query.get("quality").map(String::as_str),
            Some("magic_acappella")
        );
    }

    #[test]
    fn search_lyric_has_no_default_params_or_signature() {
        let mut params = Map::new();
        params.insert("hash".to_string(), json!("HASH"));
        params.insert("duration".to_string(), json!(180000));
        let cookies = create_device_cookies();
        let spec = build_search_lyric(&params, &cookies);
        assert_eq!(spec.url, format!("{LYRICS_BASE}/v1/search"));
        let query: HashMap<String, String> = spec.query.into_iter().collect();
        assert_eq!(query.get("hash").map(String::as_str), Some("HASH"));
        assert_eq!(query.get("lrctxt").map(String::as_str), Some("1"));
        assert!(!query.contains_key("dfid"));
        assert!(!query.contains_key("signature"));
        assert!(!query.contains_key("clienttime"));
    }

    #[test]
    fn lyric_decode_handles_lrc_and_krc() {
        let body = json!({ "status": 1, "content": base64::engine::general_purpose::STANDARD.encode(b"lrc line"), "contenttype": 1 });
        let decoded = decode_lyric_body(body);
        assert_eq!(decoded["decodeContent"], json!("lrc line"));

        let plaintext = b"krc line";
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        encoder.write_all(plaintext).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut payload = vec![0u8, 0, 0, 4];
        payload.extend_from_slice(&compressed);
        for (index, byte) in payload[4..].iter_mut().enumerate() {
            *byte ^= KRC_KEY[index % KRC_KEY.len()];
        }
        let body = json!({ "status": 1, "content": base64::engine::general_purpose::STANDARD.encode(&payload), "contenttype": 0 });
        let decoded = decode_lyric_body(body);
        assert_eq!(decoded["decodeContent"], json!("krc line"));
    }

    #[test]
    fn query_string_encodes_special_characters() {
        let pairs = vec![
            ("keyword".to_string(), "周杰伦".to_string()),
            ("a=b".to_string(), "x&y".to_string()),
        ];
        let query = build_query_string(&pairs);
        assert_eq!(query, "keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&a%3Db=x%26y");
    }
}
