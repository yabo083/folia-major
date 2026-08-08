// src-tauri/src/updater.rs
// M10 自动更新：官方 tauri-plugin-updater 签名更新器，契约完全镜像
// folia-major/electron/main.cjs 的 update 状态机与 electron/updateChannels.cjs 的通道模型。
//
// 命令：get_update_status / updates_check / updates_mark_seen /
//       updates_open_release_page / updates_download / updates_quit_and_install
// 事件：update-status-changed（emit 到 "main" 窗口，名称与 Electron 完全一致）
// 状态：disabled/idle/checking/available/latest/error/downloading/downloaded/unsupported
//
// 安全设计（本 fork 无正式发布仓库与签名密钥，全部构建期注入、运行时 fail closed）：
//   - 端点映射（FOLIA_UPDATE_ENDPOINTS）、公钥（FOLIA_UPDATE_PUBKEY）、release 页 URL
//     （FOLIA_RELEASES_URL）经 option_env! 在编译期注入；任一缺失/非法即整体禁用，
//     不携带任何默认 URL（无 fake 默认值）。
//   - 签名验证强制：插件 Update::download 内部对下载内容做 verify_signature，
//     公钥缺失时配置解析直接返回 None，更新器根本不会被构造。
//   - 降级防护：仅接受严格大于当前版本的 manifest（semver 比较）。
//   - 跨通道防护：每个通道只查询自己的单端点（避免插件多端点"按序取第一个"串台），
//     且版本比较器按通道拒绝不该接受的预发布版本（realeco 拒 pre / limo 拒 alpha）。

use semver::Version;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

use crate::settings::{SettingsStore, UPDATE_CHANNEL};

// 设置键（electron/main.cjs 常量；settings.rs 仅做原样读写）。
pub const ENABLE_UPDATE_CHECK_KEY: &str = "ENABLE_UPDATE_CHECK";
pub const ENABLE_AUTO_UPDATE_KEY: &str = "ENABLE_AUTO_UPDATE";
pub const LAST_SEEN_UPDATE_VERSION_KEY: &str = "LAST_SEEN_UPDATE_VERSION";

const UPDATE_STATUS_CHANGED_EVENT: &str = "update-status-changed";
const MAIN_WINDOW_LABEL: &str = "main";
const STARTUP_CHECK_DELAY_MS: u64 = 4500;
const UPDATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// 构建期注入的环境变量（CI 设置；本地未设置时更新器 fail closed）。
// 名称即 option_env! 的键：FOLIA_UPDATE_ENDPOINTS / FOLIA_UPDATE_PUBKEY / FOLIA_RELEASES_URL。

// ---------------------------------------------------------------------------
// 通道模型（规格来源：electron/updateChannels.cjs + test/unit/electron/updateChannels.test.ts）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChannelId {
    Realeco,
    Limo,
    Cielo,
    Internal,
}

impl ChannelId {
    fn id(self) -> &'static str {
        match self {
            ChannelId::Realeco => "realeco",
            ChannelId::Limo => "limo",
            ChannelId::Cielo => "cielo",
            ChannelId::Internal => "internal",
        }
    }

    // 契约镜像：updaterChannel 语义（latest/beta/alpha）在 Tauri 里由构建期注入的
    // 通道端点 URL（对应 latest.json/beta.json/alpha.json）承载；该方法供测试与文档对照。
    #[allow(dead_code)]
    fn updater_channel(self) -> Option<&'static str> {
        match self {
            ChannelId::Realeco => Some("latest"),
            ChannelId::Limo => Some("beta"),
            ChannelId::Cielo => Some("alpha"),
            ChannelId::Internal => None,
        }
    }

    fn update_enabled(self) -> bool {
        !matches!(self, ChannelId::Internal)
    }

    fn rolling_release_tag(self) -> Option<&'static str> {
        match self {
            ChannelId::Limo => Some("limo"),
            ChannelId::Cielo => Some("cielo"),
            _ => None,
        }
    }
}

// 镜像 normalizeVersion：去首部 v/V。
fn normalize_version(value: &str) -> String {
    let trimmed = value.trim();
    let stripped = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    stripped.to_string()
}

// 镜像 /-alpha(?:[.\-]|$)/ 与 /-beta(?:[.\-]|$)/：仅识别 "-alpha" / "-beta" 后续跟 . - 或结尾。
fn version_has_marker(version: &str, marker: &str) -> bool {
    let lowered = version.to_lowercase();
    let needle = format!("-{marker}");
    let Some(index) = lowered.find(&needle) else {
        return false;
    };
    let rest = &lowered[index + needle.len()..];
    rest.is_empty() || rest.starts_with('.') || rest.starts_with('-')
}

// 镜像 resolveReleaseChannel：显式声明的通道优先，否则按版本后缀推断。
fn resolve_release_channel(version: &str, declared: Option<&str>) -> ChannelId {
    let declared = declared.map(str::trim).unwrap_or("").to_lowercase();
    match declared.as_str() {
        "realeco" => return ChannelId::Realeco,
        "limo" => return ChannelId::Limo,
        "cielo" => return ChannelId::Cielo,
        "internal" => return ChannelId::Internal,
        _ => {}
    }
    let normalized = version.trim().to_lowercase();
    if version_has_marker(&normalized, "alpha") {
        return ChannelId::Cielo;
    }
    if version_has_marker(&normalized, "beta") {
        return ChannelId::Limo;
    }
    ChannelId::Realeco
}

// 镜像 getReleaseUrl：rolling 通道打开滚动 tag，稳定通道打开 v<版本> tag。
fn release_url(channel: ChannelId, version: &str, releases_url: &str) -> String {
    if let Some(tag) = channel.rolling_release_tag() {
        return format!("{releases_url}/tag/{tag}");
    }
    let normalized = normalize_version(version);
    if normalized.is_empty() {
        releases_url.to_string()
    } else {
        format!("{releases_url}/tag/v{normalized}")
    }
}

// 通道版本门禁（与 shared/updateChannels.mjs 的 isChannelAllowedVersion 保持一致）：
// realeco 拒预发布；limo 拒 alpha 预发布；cielo 全放行；internal 永不更新。
fn channel_allows_version(channel: ChannelId, version: &Version) -> bool {
    match channel {
        ChannelId::Realeco => version.pre.is_empty(),
        ChannelId::Limo => {
            version.pre.is_empty()
                || !version.pre.as_str().split('.').any(|identifier| {
                    let identifier = identifier.to_ascii_lowercase();
                    identifier == "alpha" || identifier.starts_with("alpha-")
                })
        }
        ChannelId::Cielo => true,
        ChannelId::Internal => false,
    }
}

// 更新判定：严格升版（防降级）+ 通道允许（防跨通道）。
fn should_update(current: &Version, remote: &Version, channel: ChannelId) -> bool {
    remote > current && channel_allows_version(channel, remote)
}

// ---------------------------------------------------------------------------
// 构建期注入配置（fail closed）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct UpdateConfiguration {
    endpoints: HashMap<String, String>,
    pubkey: String,
    releases_url: Option<String>,
}

impl UpdateConfiguration {
    fn from_build_env() -> Option<Self> {
        Self::from_parts(
            option_env!("FOLIA_UPDATE_ENDPOINTS"),
            option_env!("FOLIA_UPDATE_PUBKEY"),
            option_env!("FOLIA_RELEASES_URL"),
        )
    }

    // 纯函数：任一必需输入缺失/非法 → None（fail closed）。endpoints 必须是
    // {"realeco": "...", "limo": "...", "cielo": "..."} 且全部为 https。
    fn from_parts(
        endpoints_raw: Option<&str>,
        pubkey: Option<&str>,
        releases_url: Option<&str>,
    ) -> Option<Self> {
        let endpoints_raw = endpoints_raw?;
        let pubkey = pubkey?;
        if pubkey.trim().is_empty() {
            return None;
        }
        let endpoints: HashMap<String, String> = serde_json::from_str(endpoints_raw).ok()?;
        if endpoints.is_empty() {
            return None;
        }
        if !["realeco", "limo", "cielo"]
            .iter()
            .all(|channel| endpoints.contains_key(*channel))
        {
            return None;
        }
        if endpoints.values().any(|url| !url.starts_with("https://")) {
            return None;
        }
        let releases_url = releases_url?.trim();
        if releases_url.is_empty() || !releases_url.starts_with("https://") {
            return None;
        }
        Some(Self {
            endpoints,
            pubkey: pubkey.trim().to_string(),
            releases_url: Some(releases_url.to_string()),
        })
    }

    fn endpoint_for(&self, channel: ChannelId) -> Option<&str> {
        self.endpoints.get(channel.id()).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// 更新状态机（镜像 electron/main.cjs 的 updateState + getUpdateStatus）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DownloadProgress {
    percent: f64,
    transferred: Option<u64>,
    total: Option<u64>,
}

// 已下载包的绑定键：通道 + 代数（纯数据，供单测验证"包必须匹配当前通道/代数"规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DownloadBinding {
    channel: ChannelId,
    generation: u64,
}

impl DownloadBinding {
    fn matches(&self, channel: ChannelId, generation: u64) -> bool {
        self.channel == channel && self.generation == generation
    }
}

// 已下载待安装内容（仅 status == "downloaded" 时存在）。安装前必须验证 binding 仍匹配
// 当前通道/代数，否则拒绝安装（防跨通道、防无效化后安装旧包）。
struct StagedDownload {
    update: tauri_plugin_updater::Update,
    bytes: Vec<u8>,
    binding: DownloadBinding,
}

impl StagedDownload {
    fn matches(&self, channel: ChannelId, generation: u64) -> bool {
        self.binding.matches(channel, generation)
    }
}

// 代数计数器：每次无效化（禁用更新检查 / 切换通道）递增；进行中的操作捕获起始代数，
// 写入前比对，代次不符即放弃写入，防止旧操作覆盖新状态。
#[derive(Debug)]
struct GenerationCounter(AtomicU64);

impl GenerationCounter {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    // 无效化：使所有已捕获旧代次的进行中操作失效，返回新代数。
    fn invalidate(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
}

struct UpdateStateInner {
    status: String,
    available_version: Option<String>,
    update_url: Option<String>,
    error: Option<String>,
    last_checked_at: Option<u64>,
    download_progress: Option<DownloadProgress>,
    // 已下载待安装内容（仅 status == "downloaded" 时存在），绑定通道 + 代数。
    downloaded: Option<StagedDownload>,
}

impl Default for UpdateStateInner {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            available_version: None,
            update_url: None,
            error: None,
            last_checked_at: None,
            download_progress: None,
            downloaded: None,
        }
    }
}

pub(crate) struct UpdaterState {
    app: AppHandle,
    current_version: String,
    inner: Mutex<UpdateStateInner>,
    config: OnceLock<Option<UpdateConfiguration>>,
    // 操作代次：无效化时递增，进行中操作写入前比对。
    generation: GenerationCounter,
    // 顶层 check/download 互斥：manual/startup/settings/auto 触发串行执行，防交错。
    operation: tokio::sync::Mutex<()>,
}

impl UpdaterState {
    pub(crate) fn new(app: AppHandle) -> Self {
        let current_version = normalize_version(&app.package_info().version.to_string());
        Self {
            app,
            current_version,
            inner: Mutex::new(UpdateStateInner::default()),
            config: OnceLock::new(),
            generation: GenerationCounter::new(),
            operation: tokio::sync::Mutex::new(()),
        }
    }

    fn config(&self) -> Option<&UpdateConfiguration> {
        self.config
            .get_or_init(UpdateConfiguration::from_build_env)
            .as_ref()
    }

    fn current_channel(&self, settings: &SettingsStore) -> ChannelId {
        let declared = settings
            .get(UPDATE_CHANNEL)
            .and_then(|value| value.as_str().map(String::from));
        resolve_release_channel(&self.current_version, declared.as_deref())
    }

    fn update_check_enabled(&self, settings: &SettingsStore) -> bool {
        match settings.get(ENABLE_UPDATE_CHECK_KEY) {
            None => true,
            Some(value) => crate::settings::as_bool_value(&value),
        }
    }

    fn auto_update_enabled(&self, settings: &SettingsStore) -> bool {
        let value = settings
            .get(ENABLE_AUTO_UPDATE_KEY)
            .unwrap_or(Value::Bool(false));
        crate::settings::as_bool_value(&value)
    }

    // 镜像 getUpdateCheckSupportReason：非 Windows → system；通道未启用 → channel。
    fn support_reason(&self, channel: ChannelId) -> Option<&'static str> {
        if !platform_is_windows() {
            return Some("system");
        }
        if !channel.update_enabled() {
            return Some("channel");
        }
        None
    }

    fn update_check_supported(&self, channel: ChannelId) -> bool {
        self.support_reason(channel).is_none()
    }

    // 镜像 isAutoUpdaterSupported：check 支持 + 发布构建 + 构建期配置存在（非 dev、非未注入）。
    fn is_supported(&self, channel: ChannelId) -> bool {
        self.update_check_supported(channel) && !cfg!(debug_assertions) && self.config().is_some()
    }

    fn status(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .status
            .clone()
    }

    // 加锁改造 inner 并广播快照（闭包不持锁跨 await，无死锁）。
    fn mutate(&self, settings: &SettingsStore, f: impl FnOnce(&mut UpdateStateInner)) -> Value {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut inner);
        let snapshot = self.build_snapshot(&inner, settings);
        drop(inner);
        let _ = self
            .app
            .emit_to(MAIN_WINDOW_LABEL, UPDATE_STATUS_CHANGED_EVENT, &snapshot);
        snapshot
    }

    fn snapshot(&self, settings: &SettingsStore) -> Value {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.build_snapshot(&inner, settings)
    }

    fn set_error_status(&self, settings: &SettingsStore, message: &str) {
        self.mutate(settings, |inner| {
            inner.status = "error".into();
            inner.error = Some(message.to_string());
            inner.download_progress = None;
        });
    }

    // 当前代数（进行中操作写入前比对）。
    fn generation(&self) -> u64 {
        self.generation.current()
    }

    // 使当前已下载包/检查结果失效（禁用检查 / 切换通道时调用）：递增代数 + 清空状态。
    // 返回新代数，供进行中操作比对。
    fn invalidate(
        &self,
        settings: &SettingsStore,
        status: &str,
        error: Option<String>,
        update_url: Option<String>,
    ) -> u64 {
        let mut generation = 0;
        self.mutate(settings, |inner| {
            // 在 inner 锁内递增代数，使"检查代次"与"清空"原子化：进行中旧操作不可能在
            // 代次检查通过之后、清空完成之前写入陈旧数据。
            generation = self.generation.invalidate();
            apply_invalidation(inner, status, error, update_url);
        });
        generation
    }

    // 仅在代次仍是当前代次时应用变更并广播；否则返回 None（进行中的旧操作放弃写入，
    // 不覆盖已无效化 / 切换通道后的新状态）。
    fn mutate_if_current(
        &self,
        settings: &SettingsStore,
        generation: u64,
        f: impl FnOnce(&mut UpdateStateInner),
    ) -> Option<Value> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.generation() != generation {
            return None;
        }
        f(&mut inner);
        let snapshot = self.build_snapshot(&inner, settings);
        drop(inner);
        let _ = self
            .app
            .emit_to(MAIN_WINDOW_LABEL, UPDATE_STATUS_CHANGED_EVENT, &snapshot);
        Some(snapshot)
    }

    // 进行中操作的 error 状态（同 set_error_status，但仅在代次仍有效时写入）。
    fn set_error_status_if_current(
        &self,
        settings: &SettingsStore,
        generation: u64,
        message: &str,
    ) {
        let _ = self.mutate_if_current(settings, generation, |inner| {
            inner.status = "error".into();
            inner.error = Some(message.to_string());
            inner.download_progress = None;
        });
    }

    fn take_current_download(&self, settings: &SettingsStore) -> Option<StagedDownload> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let channel = self.current_channel(settings);
        let generation = self.generation();
        let download = inner.downloaded.take()?;
        if download.matches(channel, generation) {
            Some(download)
        } else {
            None
        }
    }

    // 构建与 Electron getUpdateStatus 同形状的 JSON（不含 downloaded 内部字段）。
    fn build_snapshot(&self, inner: &UpdateStateInner, settings: &SettingsStore) -> Value {
        let channel = self.current_channel(settings);
        let available_version = inner.available_version.clone();
        let last_seen_version = settings
            .get(LAST_SEEN_UPDATE_VERSION_KEY)
            .and_then(|value| value.as_str().map(String::from));
        let update_seen = available_version
            .as_ref()
            .is_some_and(|version| last_seen_version.as_deref() == Some(version.as_str()));
        let update_url = inner
            .update_url
            .clone()
            .or_else(|| self.config().and_then(|config| config.releases_url.clone()));

        json!({
            "status": inner.status,
            "supported": self.is_supported(channel),
            "updateCheckSupported": self.update_check_supported(channel),
            "updateCheckSupportReason": self.support_reason(channel),
            "platform": platform_name(),
            "updateCheckEnabled": self.update_check_enabled(settings),
            "autoUpdateEnabled": self.auto_update_enabled(settings),
            "currentVersion": self.current_version,
            "availableVersion": available_version,
            "updateUrl": update_url,
            "error": inner.error,
            "lastCheckedAt": inner.last_checked_at,
            "lastSeenVersion": last_seen_version,
            "updateSeen": update_seen,
            "downloadProgress": inner.download_progress.as_ref().map(|progress| json!({
                "percent": progress.percent,
                "transferred": progress.transferred,
                "total": progress.total,
            })),
        })
    }
}

// 纯函数：应用一次无效化（清空可用版本/进度/已下载包并置状态）。供 invalidate 与单测使用。
fn apply_invalidation(
    inner: &mut UpdateStateInner,
    status: &str,
    error: Option<String>,
    update_url: Option<String>,
) {
    inner.status = status.into();
    inner.error = error;
    inner.available_version = None;
    inner.update_url = update_url;
    inner.download_progress = None;
    inner.downloaded = None;
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

fn platform_is_windows() -> bool {
    cfg!(target_os = "windows")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 更新器构造（单端点 + 公钥 + 通道比较器；任何缺失都 fail closed）
// ---------------------------------------------------------------------------

fn build_updater(
    app: &AppHandle,
    channel: ChannelId,
    endpoint: &str,
    config: &UpdateConfiguration,
) -> Result<tauri_plugin_updater::Updater, String> {
    let endpoint_url =
        url::Url::parse(endpoint).map_err(|error| format!("invalid update endpoint: {error}"))?;
    app.updater_builder()
        .endpoints(vec![endpoint_url])
        .map_err(|error| format!("invalid update endpoint: {error}"))?
        .pubkey(config.pubkey.clone())
        .version_comparator(move |current, remote| {
            should_update(&current, &remote.version, channel)
        })
        .timeout(UPDATE_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("updater is not configured: {error}"))
}

// ---------------------------------------------------------------------------
// 检查 / 下载流程（镜像 checkForUpdates / downloadAvailableUpdate）
// ---------------------------------------------------------------------------

// 顶层检查入口：持 operation 互斥锁，确保 manual/startup/settings/auto 触发串行执行。
async fn check_for_updates(app: &AppHandle, manual: bool) -> Result<(), String> {
    let state = app.state::<UpdaterState>();
    let _operation = state.operation.lock().await;
    check_for_updates_unguarded(app, manual).await
}

async fn check_for_updates_unguarded(app: &AppHandle, manual: bool) -> Result<(), String> {
    let state = app.state::<UpdaterState>();
    let settings = app.state::<SettingsStore>();
    let generation = state.generation();
    let channel = state.current_channel(&settings);

    if !manual && !state.update_check_enabled(&settings) {
        state.invalidate(&settings, "disabled", None, None);
        return Ok(());
    }
    if !state.update_check_supported(channel) {
        state.invalidate(&settings, "unsupported", None, None);
        return Ok(());
    }
    if !state.is_supported(channel) {
        state.invalidate(&settings, "idle", None, None);
        return Ok(());
    }

    let Some(config) = state.config().cloned() else {
        state.set_error_status_if_current(
            &settings,
            generation,
            "Updater is not configured for this build.",
        );
        return Ok(());
    };
    let Some(endpoint) = config.endpoint_for(channel).map(String::from) else {
        state.set_error_status_if_current(
            &settings,
            generation,
            "No update endpoint is configured for the current channel.",
        );
        return Ok(());
    };

    let updater = match build_updater(app, channel, &endpoint, &config) {
        Ok(updater) => updater,
        Err(error) => {
            state.set_error_status_if_current(&settings, generation, &error);
            return Ok(());
        }
    };

    if state
        .mutate_if_current(&settings, generation, |inner| {
            inner.status = "checking".into();
            inner.error = None;
            inner.download_progress = None;
        })
        .is_none()
    {
        // 进行期间已被无效化（禁用检查 / 切换通道）：放弃本次检查。
        return Ok(());
    }

    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            let update_url = config
                .releases_url
                .as_deref()
                .map(|releases_url| release_url(channel, &version, releases_url));
            // 新发现的版本与已下载包不同 → 旧包作废（防安装过时版本）。
            let obsolete_download = state
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .downloaded
                .as_ref()
                .is_some_and(|download| download.update.version != update.version);
            let applied = state.mutate_if_current(&settings, generation, |inner| {
                inner.status = "available".into();
                inner.available_version = Some(version);
                inner.update_url = update_url;
                inner.error = None;
                inner.last_checked_at = Some(now_ms());
                inner.download_progress = None;
                if obsolete_download {
                    inner.downloaded = None;
                }
            });
            if applied.is_some() && state.auto_update_enabled(&settings) {
                // 直接调用无锁实现：operation 互斥锁已由 check_for_updates 持有，避免死锁。
                // Box::pin 打破 check/download 相互递归调用造成的无限大小 future。
                let _ = Box::pin(download_unguarded(app)).await;
            }
        }
        Ok(None) => {
            let _ = state.mutate_if_current(&settings, generation, |inner| {
                inner.status = "latest".into();
                inner.available_version = None;
                inner.update_url = config.releases_url.clone();
                inner.error = None;
                inner.last_checked_at = Some(now_ms());
                inner.download_progress = None;
                // 无新版本：清理作废的旧下载包。
                inner.downloaded = None;
            });
        }
        Err(error) => {
            state.set_error_status_if_current(
                &settings,
                generation,
                &format!("update check failed: {error}"),
            );
        }
    }
    Ok(())
}

// 顶层下载入口：持 operation 互斥锁，确保与检查/其他下载串行执行。
async fn download(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<UpdaterState>();
    let _operation = state.operation.lock().await;
    download_unguarded(app).await
}

async fn download_unguarded(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<UpdaterState>();
    let settings = app.state::<SettingsStore>();
    let generation = state.generation();
    let channel = state.current_channel(&settings);

    if !state.is_supported(channel) {
        state.invalidate(&settings, "unsupported", None, None);
        return Ok(());
    }
    if state.status() == "downloading" {
        return Ok(());
    }
    // 镜像 downloadAvailableUpdate：无已知可用版本时先手动检查一次
    // （直接调用无锁实现，避免重入 operation 锁导致死锁）。
    if state
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .available_version
        .is_none()
    {
        let _ = Box::pin(check_for_updates_unguarded(app, true)).await;
        if state.generation() != generation {
            // 内部检查期间被无效化：放弃本次下载。
            return Ok(());
        }
    }
    if state
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .available_version
        .is_none()
    {
        return Ok(());
    }

    let Some(config) = state.config().cloned() else {
        state.set_error_status_if_current(
            &settings,
            generation,
            "Updater is not configured for this build.",
        );
        return Ok(());
    };
    let Some(endpoint) = config.endpoint_for(channel).map(String::from) else {
        state.set_error_status_if_current(
            &settings,
            generation,
            "No update endpoint is configured for the current channel.",
        );
        return Ok(());
    };
    let updater = match build_updater(app, channel, &endpoint, &config) {
        Ok(updater) => updater,
        Err(error) => {
            state.set_error_status_if_current(&settings, generation, &error);
            return Ok(());
        }
    };

    // 重新 check 拿到 Update 对象（插件在 download 内部强制校验签名）。
    let update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => {
            let _ = state.mutate_if_current(&settings, generation, |inner| {
                inner.status = "latest".into();
                inner.available_version = None;
                inner.error = None;
                inner.download_progress = None;
                inner.downloaded = None;
            });
            return Ok(());
        }
        Err(error) => {
            state.set_error_status_if_current(
                &settings,
                generation,
                &format!("update check failed: {error}"),
            );
            return Ok(());
        }
    };
    let version = update.version.clone();

    if state
        .mutate_if_current(&settings, generation, |inner| {
            inner.status = "downloading".into();
            inner.error = None;
            inner.available_version = Some(version.clone());
            inner.download_progress = Some(DownloadProgress {
                percent: 0.0,
                transferred: Some(0),
                total: None,
            });
        })
        .is_none()
    {
        // 进行期间已被无效化：不启动下载。
        return Ok(());
    }

    let progress_app = app.clone();
    let mut transferred: u64 = 0;
    let download_result = update
        .download(
            move |chunk_length, total| {
                transferred += chunk_length as u64;
                if let Some(state) = progress_app.try_state::<UpdaterState>() {
                    if let Some(settings) = progress_app.try_state::<SettingsStore>() {
                        // 进度回调只写当前代次：已无效化的旧下载不再污染新状态。
                        state.mutate_if_current(&settings, generation, |inner| {
                            inner.download_progress = Some(DownloadProgress {
                                percent: total
                                    .filter(|total_bytes| *total_bytes > 0)
                                    .map(|total_bytes| {
                                        (transferred as f64 / total_bytes as f64) * 100.0
                                    })
                                    .unwrap_or(0.0),
                                transferred: Some(transferred),
                                total,
                            });
                        });
                    }
                }
            },
            || {},
        )
        .await;

    match download_result {
        Ok(bytes) => {
            let _ = state.mutate_if_current(&settings, generation, |inner| {
                inner.status = "downloaded".into();
                inner.available_version = Some(version);
                inner.error = None;
                inner.download_progress = None;
                inner.downloaded = Some(StagedDownload {
                    update,
                    bytes,
                    binding: DownloadBinding {
                        channel,
                        generation,
                    },
                });
            });
        }
        Err(error) => {
            state.set_error_status_if_current(
                &settings,
                generation,
                &format!("update download failed: {error}"),
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 命令（契约：public/electron-shim.js 的 window.electron 映射）
// ---------------------------------------------------------------------------

#[tauri::command]
// 返回当前更新状态快照（Electron getUpdateStatus 同形状）。
pub fn get_update_status(
    app: AppHandle,
    settings: State<'_, SettingsStore>,
) -> Result<Value, String> {
    Ok(app.state::<UpdaterState>().snapshot(&settings))
}

#[tauri::command]
// 手动检查更新；返回最终状态（镜像 checkForUpdates({ manual: true }) 的 IPC 语义）。
pub async fn updates_check(
    app: AppHandle,
    settings: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let state = app.state::<UpdaterState>();
    let _operation = state.operation.lock().await;
    let status = state.status();
    if status == "checking" || status == "downloading" {
        return Ok(state.snapshot(&settings));
    }
    let _ = check_for_updates_unguarded(&app, true).await;
    Ok(state.snapshot(&settings))
}

#[tauri::command]
// 记录"已看到"的版本（LAST_SEEN_UPDATE_VERSION），并重新广播状态。
pub fn updates_mark_seen(
    version: Option<String>,
    app: AppHandle,
    settings: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let state = app.state::<UpdaterState>();
    let target = version
        .as_deref()
        .map(normalize_version)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            state
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .available_version
                .clone()
        });
    if let Some(seen_version) = target {
        settings.set(
            LAST_SEEN_UPDATE_VERSION_KEY.to_string(),
            Value::String(seen_version),
        )?;
    }
    let snapshot = state.snapshot(&settings);
    let _ = app.emit_to(MAIN_WINDOW_LABEL, UPDATE_STATUS_CHANGED_EVENT, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
// 打开当前通道的 release 页（rolling tag 或 v<version> tag）；未配置 release URL 时返回 false。
pub fn updates_open_release_page(
    version: Option<String>,
    app: AppHandle,
    settings: State<'_, SettingsStore>,
) -> Result<bool, String> {
    let state = app.state::<UpdaterState>();
    let channel = state.current_channel(&settings);
    let config = state.config();
    let url = match version
        .as_deref()
        .map(normalize_version)
        .filter(|value| !value.is_empty())
    {
        Some(normalized_version) => config
            .and_then(|config| config.releases_url.as_deref())
            .map(|releases_url| release_url(channel, &normalized_version, releases_url)),
        None => state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .update_url
            .clone()
            .or_else(|| config.and_then(|config| config.releases_url.clone())),
    };
    match url {
        Some(target_url) => crate::network::open_external_url(target_url, app),
        None => Ok(false),
    }
}

#[tauri::command]
// 下载已发现的更新（下载结束由插件完成签名验证）；返回最终状态。
pub async fn updates_download(
    app: AppHandle,
    settings: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let state = app.state::<UpdaterState>();
    let _operation = state.operation.lock().await;
    let _ = download_unguarded(&app).await;
    Ok(state.snapshot(&settings))
}

#[tauri::command]
// 退出并安装已下载更新（NSIS 安装器启动后进程退出）；未下载完成返回 false。
// 下载包必须匹配当前通道/代数，否则视为失效并拒绝安装。
pub fn updates_quit_and_install(
    app: AppHandle,
    settings: State<'_, SettingsStore>,
) -> Result<bool, String> {
    let state = app.state::<UpdaterState>();
    let Some(download) = state.take_current_download(&settings) else {
        return Ok(false);
    };
    match download.update.install(download.bytes) {
        Ok(()) => Ok(true),
        Err(error) => {
            state.set_error_status(&settings, &format!("update install failed: {error}"));
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// 生命周期钩子（lib.rs setup 与 settings.rs save_settings 调用）
// ---------------------------------------------------------------------------

// 镜像 scheduleStartupUpdateCheck：按设置/支持度设置初始状态，支持则延迟 4.5s 后台检查。
pub fn schedule_startup_check(app: &AppHandle) {
    let state = app.state::<UpdaterState>();
    let settings = app.state::<SettingsStore>();
    let channel = state.current_channel(&settings);

    if !state.update_check_enabled(&settings) {
        state.invalidate(&settings, "disabled", None, None);
        return;
    }
    if !state.update_check_supported(channel) {
        state.invalidate(&settings, "unsupported", None, None);
        return;
    }
    if !state.is_supported(channel) {
        state.invalidate(&settings, "idle", None, None);
        return;
    }

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(STARTUP_CHECK_DELAY_MS)).await;
        let _ = check_for_updates(&handle, false).await;
    });
}

// 镜像 main.cjs save-settings 中 ENABLE_UPDATE_CHECK / ENABLE_AUTO_UPDATE / UPDATE_CHANNEL 的联动。
pub fn on_setting_saved(app: &AppHandle, key: &str, value: &Value) {
    let state = app.state::<UpdaterState>();
    let settings = app.state::<SettingsStore>();
    let channel = state.current_channel(&settings);

    match key {
        ENABLE_UPDATE_CHECK_KEY => {
            if crate::settings::as_bool_value(value) {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = check_for_updates(&handle, false).await;
                });
            } else {
                // 禁用检查：使已有检查结果与已下载包失效（递增代数，进行中的旧操作无法再写入）。
                state.invalidate(&settings, "disabled", None, None);
            }
        }
        ENABLE_AUTO_UPDATE_KEY => {
            let has_available = state
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .available_version
                .is_some();
            if crate::settings::as_bool_value(value) && has_available {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = download(&handle).await;
                });
            } else {
                let _ = state.mutate(&settings, |_inner| {});
            }
        }
        UPDATE_CHANNEL => {
            let next_status =
                if state.update_check_enabled(&settings) && state.update_check_supported(channel) {
                    "idle"
                } else {
                    "unsupported"
                };
            let releases_url = state
                .config()
                .and_then(|config| config.releases_url.clone());
            // 切换通道：旧通道的检查结果与已下载包一律作废（递增代数）。
            state.invalidate(&settings, next_status, None, releases_url);
            if state.update_check_enabled(&settings) && state.is_supported(channel) {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = check_for_updates(&handle, false).await;
                });
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑单测（规格来源：test/unit/electron/updateChannels.test.ts）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(value: &str) -> Version {
        Version::parse(value).unwrap()
    }

    #[test]
    fn maps_versions_to_lanes_like_the_reference() {
        assert_eq!(resolve_release_channel("0.7.0", None), ChannelId::Realeco);
        assert_eq!(
            resolve_release_channel("0.7.0-beta.123", None),
            ChannelId::Limo
        );
        assert_eq!(
            resolve_release_channel("0.7.0-alpha.123", None),
            ChannelId::Cielo
        );
    }

    #[test]
    fn packaged_metadata_wins_over_version_suffix() {
        assert_eq!(
            resolve_release_channel("0.7.0-beta.1", Some("internal")),
            ChannelId::Internal
        );
        assert_eq!(
            resolve_release_channel("0.7.0-alpha.1", Some("limo")),
            ChannelId::Limo
        );
        assert_eq!(
            resolve_release_channel("0.7.0", Some("cielo")),
            ChannelId::Cielo
        );
    }

    #[test]
    fn channel_updater_lane_and_capabilities() {
        assert_eq!(ChannelId::Realeco.updater_channel(), Some("latest"));
        assert_eq!(ChannelId::Limo.updater_channel(), Some("beta"));
        assert_eq!(ChannelId::Cielo.updater_channel(), Some("alpha"));
        assert_eq!(ChannelId::Internal.updater_channel(), None);
        assert!(!ChannelId::Internal.update_enabled());
        assert_eq!(ChannelId::Limo.rolling_release_tag(), Some("limo"));
        assert_eq!(ChannelId::Cielo.rolling_release_tag(), Some("cielo"));
        assert_eq!(ChannelId::Realeco.rolling_release_tag(), None);
    }

    #[test]
    fn release_urls_open_rolling_tags_for_prerelease_lanes() {
        let releases_url = "https://github.com/chthollyphile/folia-major/releases";
        assert_eq!(
            release_url(ChannelId::Limo, "0.7.0-beta.123", releases_url),
            format!("{releases_url}/tag/limo")
        );
        assert_eq!(
            release_url(ChannelId::Cielo, "0.7.0-alpha.123", releases_url),
            format!("{releases_url}/tag/cielo")
        );
        assert_eq!(
            release_url(ChannelId::Realeco, "0.7.0", releases_url),
            format!("{releases_url}/tag/v0.7.0")
        );
        assert_eq!(
            release_url(ChannelId::Realeco, "", releases_url),
            releases_url
        );
    }

    #[test]
    fn normalize_version_strips_v_prefix_once() {
        assert_eq!(normalize_version("0.7.0"), "0.7.0");
        assert_eq!(normalize_version("v0.7.0"), "0.7.0");
        assert_eq!(normalize_version("V0.7.0-beta.1"), "0.7.0-beta.1");
        assert_eq!(normalize_version("  v0.7.0  "), "0.7.0");
    }

    #[test]
    fn never_downgrades_and_ignores_equal_versions() {
        assert!(!should_update(
            &v("0.7.0"),
            &v("0.6.12"),
            ChannelId::Realeco
        ));
        assert!(!should_update(&v("0.7.0"), &v("0.7.0"), ChannelId::Realeco));
        assert!(!should_update(
            &v("0.7.0"),
            &v("0.7.0-beta.1"),
            ChannelId::Realeco
        ));
        assert!(!should_update(
            &v("0.7.0"),
            &v("0.7.0-alpha.1"),
            ChannelId::Cielo
        ));
        // beta → stable 是升版（稳定版高于同版本预发布），允许。
        assert!(should_update(
            &v("0.7.0-beta.1"),
            &v("0.7.0"),
            ChannelId::Limo
        ));
    }

    #[test]
    fn stable_channel_rejects_prereleases() {
        assert!(should_update(&v("0.6.12"), &v("0.7.0"), ChannelId::Realeco));
        assert!(!should_update(
            &v("0.6.12"),
            &v("0.7.0-beta.1"),
            ChannelId::Realeco
        ));
        assert!(!should_update(
            &v("0.6.12"),
            &v("0.7.0-alpha.1"),
            ChannelId::Realeco
        ));
    }

    #[test]
    fn beta_channel_accepts_stable_and_beta_but_rejects_alpha() {
        assert!(should_update(&v("0.6.12"), &v("0.7.0"), ChannelId::Limo));
        assert!(should_update(
            &v("0.6.12"),
            &v("0.7.0-beta.1"),
            ChannelId::Limo
        ));
        assert!(!should_update(
            &v("0.6.12"),
            &v("0.7.0-alpha.1"),
            ChannelId::Limo
        ));
        assert!(!should_update(
            &v("0.6.12"),
            &v("0.7.0-alpha-1"),
            ChannelId::Limo
        ));
        assert!(!should_update(
            &v("0.6.12"),
            &v("0.7.0-ALPHA.1"),
            ChannelId::Limo
        ));
    }

    #[test]
    fn alpha_channel_accepts_every_newer_version() {
        assert!(should_update(&v("0.6.12"), &v("0.7.0"), ChannelId::Cielo));
        assert!(should_update(
            &v("0.6.12"),
            &v("0.7.0-beta.1"),
            ChannelId::Cielo
        ));
        assert!(should_update(
            &v("0.6.12"),
            &v("0.7.0-alpha.1"),
            ChannelId::Cielo
        ));
    }

    #[test]
    fn internal_channel_never_updates() {
        assert!(!should_update(
            &v("0.6.12"),
            &v("0.7.0"),
            ChannelId::Internal
        ));
    }

    #[test]
    fn configuration_fails_closed_when_any_required_input_is_missing() {
        assert!(UpdateConfiguration::from_parts(None, Some("pubkey"), None).is_none());
        assert!(UpdateConfiguration::from_parts(Some("{}"), None, None).is_none());
        assert!(UpdateConfiguration::from_parts(Some("{}"), Some("  "), None).is_none());
        assert!(UpdateConfiguration::from_parts(Some("not json"), Some("pubkey"), None).is_none());
        assert!(UpdateConfiguration::from_parts(Some("{}"), Some("pubkey"), None).is_none());
        assert!(UpdateConfiguration::from_parts(
            Some("{\"other\":\"https://example.com/x.json\"}"),
            Some("pubkey"),
            None,
        )
        .is_none());
    }

    #[test]
    fn configuration_rejects_non_https_endpoints() {
        assert!(UpdateConfiguration::from_parts(
            Some("{\"realeco\":\"http://insecure.example/latest.json\"}"),
            Some("pubkey"),
            None,
        )
        .is_none());
    }

    #[test]
    fn configuration_requires_https_releases_url() {
        let endpoints = Some("{\"realeco\":\"https://releases.example.com/latest.json\"}");
        assert!(UpdateConfiguration::from_parts(endpoints, Some("pubkey"), None).is_none());
        assert!(UpdateConfiguration::from_parts(
            endpoints,
            Some("pubkey"),
            Some("http://github.example/releases"),
        )
        .is_none());
    }

    #[test]
    fn initial_update_state_is_idle() {
        assert_eq!(UpdateStateInner::default().status, "idle");
    }

    #[test]
    fn configuration_parses_channel_endpoints() {
        let config = UpdateConfiguration::from_parts(
            Some(
                "{\"realeco\":\"https://releases.example.com/latest.json\",\
                 \"limo\":\"https://releases.example.com/beta.json\",\
                 \"cielo\":\"https://releases.example.com/alpha.json\"}",
            ),
            Some("cHVibGljLWtleQ=="),
            Some("https://github.com/chthollyphile/folia-major/releases"),
        )
        .expect("valid configuration");

        assert_eq!(
            config.endpoint_for(ChannelId::Realeco),
            Some("https://releases.example.com/latest.json")
        );
        assert_eq!(
            config.endpoint_for(ChannelId::Limo),
            Some("https://releases.example.com/beta.json")
        );
        assert_eq!(
            config.endpoint_for(ChannelId::Cielo),
            Some("https://releases.example.com/alpha.json")
        );
        assert_eq!(config.endpoint_for(ChannelId::Internal), None);
        assert_eq!(
            config.releases_url.as_deref(),
            Some("https://github.com/chthollyphile/folia-major/releases")
        );
    }

    #[test]
    fn downloaded_package_binding_rejects_other_channels() {
        // 在 limo 通道下载的包绑定 limo：同通道同代次可安装，切到其他通道一律拒绝。
        let binding = DownloadBinding {
            channel: ChannelId::Limo,
            generation: 3,
        };
        assert!(binding.matches(ChannelId::Limo, 3));
        assert!(!binding.matches(ChannelId::Realeco, 3));
        assert!(!binding.matches(ChannelId::Cielo, 3));
        assert!(!binding.matches(ChannelId::Internal, 3));
    }

    #[test]
    fn downloaded_package_binding_rejects_stale_generation() {
        // 绑定代数 1 的包：无效化（禁用检查/切通道）前捕获的代次与之后的新代次都不可安装。
        let binding = DownloadBinding {
            channel: ChannelId::Realeco,
            generation: 1,
        };
        assert!(binding.matches(ChannelId::Realeco, 1));
        assert!(!binding.matches(ChannelId::Realeco, 0));
        assert!(!binding.matches(ChannelId::Realeco, 2));
    }

    #[test]
    fn invalidation_orphans_operations_captured_before_it() {
        // 进行中操作捕获起始代数；任何一次无效化后该操作都不再能写入状态。
        let counter = GenerationCounter::new();
        let in_flight = counter.current();
        assert_eq!(in_flight, counter.current());

        counter.invalidate();
        assert_ne!(in_flight, counter.current());

        // 连续无效化同样使更早的代次失效。
        let second_op = counter.current();
        counter.invalidate();
        counter.invalidate();
        assert_ne!(in_flight, counter.current());
        assert_ne!(second_op, counter.current());

        // 无效化后新开始的操作捕获的是当前代数，仍然有效。
        let fresh_op = counter.current();
        assert_eq!(fresh_op, counter.current());
    }

    #[test]
    fn invalidation_clears_available_version_progress_and_downloaded() {
        // 禁用更新检查/切换通道时：状态置位，可用版本、URL、错误、进度、已下载包全部清空。
        let mut inner = UpdateStateInner {
            status: "available".into(),
            available_version: Some("0.7.0".into()),
            update_url: Some("https://releases.example.com/tag/v0.7.0".into()),
            error: Some("stale".into()),
            last_checked_at: Some(42),
            download_progress: Some(DownloadProgress {
                percent: 50.0,
                transferred: Some(5),
                total: Some(10),
            }),
            downloaded: None,
        };
        apply_invalidation(&mut inner, "disabled", None, None);
        assert_eq!(inner.status, "disabled");
        assert!(inner.available_version.is_none());
        assert!(inner.update_url.is_none());
        assert!(inner.error.is_none());
        assert!(inner.download_progress.is_none());
        assert!(inner.downloaded.is_none());
        // last_checked_at 保留（镜像 Electron：禁用/切通道不清空最后检查时间）。
        assert_eq!(inner.last_checked_at, Some(42));
    }

    #[test]
    fn invalidation_keeps_channel_release_url() {
        // 切换通道后展示新通道的 release 页 URL，其余状态被清空。
        let mut inner = UpdateStateInner::default();
        apply_invalidation(
            &mut inner,
            "idle",
            None,
            Some("https://releases.example.com".into()),
        );
        assert_eq!(inner.status, "idle");
        assert_eq!(
            inner.update_url.as_deref(),
            Some("https://releases.example.com")
        );
        assert!(inner.available_version.is_none());
        assert!(inner.download_progress.is_none());
        assert!(inner.downloaded.is_none());
    }
}
