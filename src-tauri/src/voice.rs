//! M8 voice-input pause: watches Windows microphone capture state (system voice
//! typing, IME voice input) and notifies the renderer so playback can pause and
//! resume automatically.
//!
//! Mirrors `folia-major/electron/voiceInputPause.cjs`: the microphone
//! `ConsentStore` registry entries are queried (`reg query ... /s`), the Folia
//! process itself is excluded from the "in use" check, and consecutive samples
//! (2 to start, 3 to stop) debounce brief mic touches. Polling runs on a
//! background thread only while the setting is enabled (1s interval, matching
//! Electron), never at high frequency.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::settings::{SettingsStore, VOICE_INPUT_PAUSE_ENABLED};

pub const MICROPHONE_CONSENT_STORE_KEY: &str = r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
const START_CONFIRM_SAMPLES: u32 = 2;
const STOP_CONFIRM_SAMPLES: u32 = 3;
const REG_QUERY_TIMEOUT_MS: Duration = Duration::from_millis(5000);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// -- pure parsing (spec source: test/unit/electron/voiceInputPause.test.ts) ---

// NonPackaged consent-store subkeys encode the exe full path with '#' separators.
fn normalize_consent_store_exe_path(sub_key: &str) -> String {
    sub_key.replace('#', "\\").to_lowercase()
}

fn normalize_exe_path(value: &str) -> String {
    value.replace('/', "\\").to_lowercase()
}

fn is_own_process_key(key_path: &str, own_exe_path: &str) -> bool {
    let normalized_own = normalize_exe_path(own_exe_path);
    if normalized_own.is_empty() {
        return false;
    }
    let marker = "\\nonpackaged\\";
    let lowered = key_path.to_lowercase();
    let Some(marker_index) = lowered.find(marker) else {
        return false;
    };
    let sub_key = &key_path[marker_index + marker.len()..];
    normalize_consent_store_exe_path(sub_key) == normalized_own
}

/// Parses `reg query <microphone ConsentStore> /s` output. An app is actively
/// capturing when LastUsedTimeStop == 0 while LastUsedTimeStart != 0.
pub fn parse_microphone_consent_store_in_use(output: &str, own_exe_path: &str) -> bool {
    #[derive(Default)]
    struct Entry {
        key_path: String,
        start: Option<u64>,
        stop: Option<u64>,
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<Entry> = None;

    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.to_ascii_uppercase().starts_with("HKEY_") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(Entry {
                key_path: line.to_string(),
                start: None,
                stop: None,
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 3 {
            let name = fields[0];
            let hex = fields[2];
            if (name == "LastUsedTimeStart" || name == "LastUsedTimeStop")
                && fields[1] == "REG_QWORD"
                && hex.starts_with("0x")
            {
                if let Ok(value) = u64::from_str_radix(&hex[2..], 16) {
                    if name == "LastUsedTimeStart" {
                        entry.start = Some(value);
                    } else {
                        entry.stop = Some(value);
                    }
                }
            }
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }

    entries.into_iter().any(|entry| {
        !is_own_process_key(&entry.key_path, own_exe_path)
            && entry.start.is_some()
            && entry.start != Some(0)
            && entry.stop == Some(0)
    })
}

// -- debounce state machine ----------------------------------------------------

/// Consecutive-sample debounce state shared with the monitor thread.
#[derive(Debug, Clone, PartialEq)]
pub struct DebounceState {
    pub active: bool,
    pub pending_sample: Option<bool>,
    pub confirm_count: u32,
}

impl Default for DebounceState {
    fn default() -> Self {
        Self {
            active: false,
            pending_sample: None,
            confirm_count: 0,
        }
    }
}

/// Applies one microphone sample; returns true when the `active` flag flipped.
/// Mirrors Electron `applySample` (START=2 / STOP=3 consecutive samples).
pub fn apply_sample_to(state: &mut DebounceState, in_use: bool) -> bool {
    if in_use == state.active {
        state.pending_sample = None;
        state.confirm_count = 0;
        return false;
    }
    if state.pending_sample != Some(in_use) {
        state.pending_sample = Some(in_use);
        state.confirm_count = 1;
    } else {
        state.confirm_count += 1;
    }
    let needed = if in_use {
        START_CONFIRM_SAMPLES
    } else {
        STOP_CONFIRM_SAMPLES
    };
    if state.confirm_count >= needed {
        state.active = in_use;
        state.pending_sample = None;
        state.confirm_count = 0;
        return true;
    }
    false
}

// -- production query ------------------------------------------------------------

/// Runs `reg query <ConsentStore> /s`; `None` on any failure (mirrors Electron
/// resolving `null` when `execFile` errors).
fn query_windows_microphone_in_use(own_exe_path: &str) -> Option<bool> {
    let mut command = std::process::Command::new("reg");
    command.args(["query", MICROPHONE_CONSENT_STORE_KEY, "/s"]);
    let output = run_with_drain(command, REG_QUERY_TIMEOUT_MS)?;
    Some(parse_microphone_consent_store_in_use(&output, own_exe_path))
}

/// Runs a command while concurrently draining its stdout, so a child writing
/// more than the anonymous-pipe buffer (~64KB, e.g. a big `reg query /s`)
/// never blocks the child (and thus never trips the timeout). stderr is
/// discarded (`Stdio::null`). On exit (normal, error or kill) the reader
/// thread is joined so the full output is parsed; `None` on spawn failure,
/// non-zero exit or timeout.
fn run_with_drain(mut command: std::process::Command, timeout: Duration) -> Option<String> {
    #[cfg(windows)]
    {
        use std::io::Read;
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;

        command.creation_flags(CREATE_NO_WINDOW);
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn().ok()?;
        let mut reader = child.stdout.take()?;
        // 启动后立即在独立线程排空 stdout，进程不会再因 pipe 满而阻塞。
        let reader_handle = std::thread::spawn(move || {
            let mut output = String::new();
            let _ = reader.read_to_string(&mut output);
            output
        });

        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader_handle.join();
                    return None;
                }
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader_handle.join();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let stdout = reader_handle.join().unwrap_or_default();
        match status {
            Some(status) if status.success() => Some(stdout),
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (command, timeout);
        None
    }
}

// -- monitor ---------------------------------------------------------------------

#[derive(Debug)]
enum ControlMessage {
    Stop,
}

/// 每次启动/停止线程时递增；线程捕获自己那一代的编号，任何过期检查失败后
/// 立即放弃（不改 debounce 状态、不发事件），防止快速 disable→enable 时
/// 旧线程残留改状态或晚发事件。
#[derive(Debug, Clone, Copy, PartialEq)]
enum LoopAction {
    Tick,
    Stop,
}

/// 区分 recv_timeout 的两种失败：Timeout 表示轮询周期到点（照常采样），
/// Disconnected 表示发送端已消失（stop() 丢弃 tx）——此时必须退出，
/// 否则会在死通道上空转采样。
fn classify_recv(result: Result<ControlMessage, mpsc::RecvTimeoutError>) -> LoopAction {
    match result {
        Ok(ControlMessage::Stop) => LoopAction::Stop,
        Err(mpsc::RecvTimeoutError::Timeout) => LoopAction::Tick,
        Err(mpsc::RecvTimeoutError::Disconnected) => LoopAction::Stop,
    }
}

/// 应用一次查询结果：本代已过期（generation != my_gen）时直接放弃——
/// 不触碰 debounce 状态、不发事件——并返回 false 让线程尽快退出。
/// 查询失败（`in_use == None`）只跳过本次采样，线程继续。
/// 代检查、状态更新和事件发送在同一把锁内完成；stop/sync_state 也在这把锁内
/// 递增代，因此旧线程绝无可能在失效操作返回后再改写状态或发出事件。
fn apply_sampled_state(
    inner: &Mutex<DebounceState>,
    generation: &AtomicU64,
    my_gen: u64,
    in_use: Option<bool>,
    is_enabled: &dyn Fn() -> bool,
    supported: bool,
    emit: &dyn Fn(&Value),
) -> bool {
    let Some(in_use) = in_use else {
        return generation.load(Ordering::SeqCst) == my_gen;
    };
    let mut state = inner.lock().unwrap();
    if generation.load(Ordering::SeqCst) != my_gen {
        return false;
    }
    let flipped = apply_sample_to(&mut state, in_use);
    if flipped {
        let payload = json!({
            "active": state.active,
            "enabled": is_enabled(),
            "supported": supported,
        });
        // Generation invalidation takes the same lock, so stop/disable cannot
        // return between this check and the event emission.
        emit(&payload);
    }
    true
}

type EmitFn = Arc<dyn Fn(&Value) + Send + Sync>;
type EnabledFn = Arc<dyn Fn() -> bool + Send + Sync>;
type QueryFn = Arc<dyn Fn(&str) -> Option<bool> + Send + Sync>;

/// Polls microphone capture state while enabled and publishes `active` flips to
/// the main window (`voice-input-state-changed`).
pub struct VoiceInputPauseMonitor {
    inner: Arc<Mutex<DebounceState>>,
    emit: EmitFn,
    is_enabled: EnabledFn,
    query_in_use: QueryFn,
    own_exe_path: String,
    supported: bool,
    poll_interval_ms: u64,
    generation: Arc<AtomicU64>,
    control: Mutex<Option<mpsc::Sender<ControlMessage>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl VoiceInputPauseMonitor {
    /// Production constructor wired to settings + the main-window event bus.
    pub fn new(app: &AppHandle) -> Self {
        let emit_app = app.clone();
        let enabled_app = app.clone();
        let emit: EmitFn = Arc::new(move |payload: &Value| {
            use tauri::Emitter;
            let _ = emit_app.emit_to("main", "voice-input-state-changed", payload.clone());
        });
        let is_enabled: EnabledFn = Arc::new(move || {
            enabled_app
                .state::<SettingsStore>()
                .get_bool(VOICE_INPUT_PAUSE_ENABLED)
        });
        let own_exe_path = std::env::current_exe()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(DebounceState::default())),
            emit,
            is_enabled,
            query_in_use: Arc::new(query_windows_microphone_in_use),
            own_exe_path,
            supported: cfg!(windows),
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            generation: Arc::new(AtomicU64::new(0)),
            control: Mutex::new(None),
            thread: Mutex::new(None),
        }
    }

    /// Test constructor with injectable dependencies.
    #[cfg(test)]
    pub fn for_test(
        is_enabled: EnabledFn,
        query_in_use: QueryFn,
        supported: bool,
        poll_interval_ms: u64,
    ) -> (Self, Arc<Mutex<Vec<Value>>>) {
        let recorded: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = recorded.clone();
        (
            Self {
                inner: Arc::new(Mutex::new(DebounceState::default())),
                emit: Arc::new(move |payload: &Value| {
                    recorder.lock().unwrap().push(payload.clone());
                }),
                is_enabled,
                query_in_use,
                own_exe_path: "C:\\Apps\\Folia.exe".to_string(),
                supported,
                poll_interval_ms,
                generation: Arc::new(AtomicU64::new(0)),
                control: Mutex::new(None),
                thread: Mutex::new(None),
            },
            recorded,
        )
    }

    pub fn get_status(&self) -> Value {
        let state = self.inner.lock().unwrap();
        json!({
            "active": state.active,
            "enabled": (self.is_enabled)(),
            "supported": self.supported,
        })
    }

    fn publish(&self) {
        let payload = self.get_status();
        (self.emit)(&payload);
    }

    #[cfg(test)]
    fn apply_sample(&self, in_use: bool) {
        let flipped = {
            let mut state = self.inner.lock().unwrap();
            apply_sample_to(&mut state, in_use)
        };
        if flipped {
            self.publish();
        }
    }

    // 独立线程：仅 enabled+supported 时由 sync_state 启动，按 poll_interval 轮询；
    // recv_timeout 即时响应 Stop/Wake，避免频繁开关时残留旧线程。
    // 每次启动领取新代（generation），旧代线程在下次检查时发现过期即退出，
    // 不会再改 debounce 状态或发事件。
    fn spawn_monitor_thread(&self) {
        let (tx, rx) = mpsc::channel::<ControlMessage>();
        let mut thread_guard = self.thread.lock().unwrap();
        if thread_guard.is_some() {
            return;
        }
        let generation = self.generation.clone();
        let my_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
        *self.control.lock().unwrap() = Some(tx);
        let handle = std::thread::spawn({
            let inner = self.inner.clone();
            let query_in_use = self.query_in_use.clone();
            let own_exe_path = self.own_exe_path.clone();
            let emit = self.emit.clone();
            let is_enabled = self.is_enabled.clone();
            let supported = self.supported;
            let poll_interval_ms = self.poll_interval_ms;
            move || {
                let tick = || {
                    let in_use = query_in_use(&own_exe_path);
                    apply_sampled_state(
                        &inner,
                        &generation,
                        my_gen,
                        in_use,
                        is_enabled.as_ref(),
                        supported,
                        emit.as_ref(),
                    )
                };
                // 启动即采样（同 Electron syncState 的 immediate tick）。
                if !tick() {
                    return;
                }
                loop {
                    let action =
                        classify_recv(rx.recv_timeout(Duration::from_millis(poll_interval_ms)));
                    match action {
                        LoopAction::Tick => {
                            if !tick() {
                                break;
                            }
                        }
                        LoopAction::Stop => break,
                    }
                }
            }
        });
        *thread_guard = Some(handle);
    }

    // 设置联动入口：启用时启动轮询线程，停用时回收线程并立即释放 active。
    pub fn sync_state(&self) -> Value {
        let should_run = self.supported && (self.is_enabled)();

        let running = self.thread.lock().unwrap().is_some();
        if should_run && !running {
            self.spawn_monitor_thread();
        } else if !should_run && running {
            // 立即推进代，使残留线程的 in-flight 采样在拿到结果后过期放弃；
            // 后台回收旧线程，避免 save_settings 被 reg query 最长 5s 阻塞。
            {
                let _state = self.inner.lock().unwrap();
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
            let handle = self.thread.lock().unwrap().take();
            let tx = self.control.lock().unwrap().take();
            if let (Some(handle), Some(tx)) = (handle, tx) {
                std::thread::spawn(move || {
                    let _ = tx.send(ControlMessage::Stop);
                    let _ = handle.join();
                });
            }
        }

        // 语音输入中关闭功能 → 立即释放 active（同 Electron 的 mid-dictation 释放）。
        let released = {
            let mut state = self.inner.lock().unwrap();
            if !should_run && state.active {
                state.active = false;
                state.pending_sample = None;
                state.confirm_count = 0;
                true
            } else {
                false
            }
        };
        if released {
            self.publish();
        }
        self.get_status()
    }

    // 应用退出：推进代使线程过期，发 Stop 并清理状态（线程随进程退出，不阻塞）。
    // 锁序统一为 inner → thread → control：spawn_monitor_thread 持 thread 锁再
    // 取 control，sync_state 停用路径同序；stop 不得反过来（control → thread），
    // 否则与并发 spawn 形成相反的嵌套顺序（AB-BA）。此处不扩大锁范围：
    // 每把锁的 guard 只在取走字段的语句内短暂持有。
    pub fn stop(&self) {
        {
            let mut state = self.inner.lock().unwrap();
            self.generation.fetch_add(1, Ordering::SeqCst);
            state.active = false;
            state.pending_sample = None;
            state.confirm_count = 0;
        }
        let handle = self.thread.lock().unwrap().take();
        let tx = self.control.lock().unwrap().take();
        if let Some(tx) = tx {
            let _ = tx.send(ControlMessage::Stop);
        }
        // JoinHandle 在此 drop（detach）；线程通过 Stop 消息/代过期退出。
        drop(handle);
    }
}

#[tauri::command]
// 返回语音输入暂停监控状态（active/enabled/supported）。
pub fn voice_input_pause_get_status(app: AppHandle) -> Result<Value, String> {
    Ok(app.state::<VoiceInputPauseMonitor>().get_status())
}

/// 设置联动：VOICE_INPUT_PAUSE_ENABLED 变更时启动/停止轮询并推送状态。
pub fn sync_state(app: &AppHandle) {
    app.state::<VoiceInputPauseMonitor>().sync_state();
}

/// 应用退出时停止轮询（best-effort）。
pub fn stop(app: &AppHandle) {
    app.state::<VoiceInputPauseMonitor>().stop();
}

// -- tests -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const CONSENT_STORE_HEADER: &str = r"HKEY_CURRENT_USER\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

    fn build_reg_output(entries: &[(&str, &str, &str)]) -> String {
        let mut lines = vec![CONSENT_STORE_HEADER.to_string()];
        for (sub_key, start, stop) in entries {
            lines.push(format!("{CONSENT_STORE_HEADER}\\{sub_key}"));
            lines.push(format!("    LastUsedTimeStart    REG_QWORD    {start}"));
            lines.push(format!("    LastUsedTimeStop    REG_QWORD    {stop}"));
            lines.push(String::new());
        }
        lines.push("End of search: 1 match(es) found.".to_string());
        lines.join("\r\n")
    }

    #[test]
    fn detects_actively_capturing_app() {
        let output = build_reg_output(&[(
            "Microsoft.Windows.ShellExperienceHost_cw5n1h2txyewy",
            "0x1dc1234abcd0000",
            "0x0",
        )]);
        assert!(parse_microphone_consent_store_in_use(
            &output,
            r"C:\Apps\Folia.exe"
        ));
    }

    #[test]
    fn ignores_apps_whose_capture_ended() {
        let output = build_reg_output(&[(
            "Microsoft.Windows.ShellExperienceHost_cw5n1h2txyewy",
            "0x1dc1234abcd0000",
            "0x1dc1234abce0000",
        )]);
        assert!(!parse_microphone_consent_store_in_use(
            &output,
            r"C:\Apps\Folia.exe"
        ));
    }

    #[test]
    fn ignores_entries_that_never_started() {
        let output = build_reg_output(&[("Some.Packaged.App_abc123", "0x0", "0x0")]);
        assert!(!parse_microphone_consent_store_in_use(
            &output,
            r"C:\Apps\Folia.exe"
        ));
    }

    #[test]
    fn excludes_the_folia_process_itself_from_nonpackaged_entries() {
        let output = build_reg_output(&[(
            "NonPackaged\\C:#Program Files#Folia#Folia.exe",
            "0x1dc1234abcd0000",
            "0x0",
        )]);
        assert!(!parse_microphone_consent_store_in_use(
            &output,
            r"C:\Program Files\Folia\Folia.exe"
        ));
    }

    #[test]
    fn still_reports_other_apps_capturing_alongside_folia() {
        let output = build_reg_output(&[
            (
                "NonPackaged\\C:#Program Files#Folia#Folia.exe",
                "0x1dc1234abcd0000",
                "0x0",
            ),
            (
                "NonPackaged\\C:#IME#sogou#SogouVoice.exe",
                "0x1dc1234abcd0000",
                "0x0",
            ),
        ]);
        assert!(parse_microphone_consent_store_in_use(
            &output,
            r"C:\Program Files\Folia\Folia.exe"
        ));
    }

    #[test]
    fn treats_empty_and_malformed_output_as_not_in_use() {
        assert!(!parse_microphone_consent_store_in_use(
            "",
            r"C:\Apps\Folia.exe"
        ));
        assert!(!parse_microphone_consent_store_in_use(
            "ERROR: The system was unable to find the specified registry key or value.",
            r"C:\Apps\Folia.exe"
        ));
    }

    #[test]
    fn consent_store_paths_normalize_forward_slashes_and_case() {
        assert_eq!(
            normalize_exe_path("C:/Apps/Folia.EXE"),
            r"c:\apps\folia.exe"
        );
        assert_eq!(
            normalize_consent_store_exe_path("C:#Apps#Folia#Folia.exe"),
            r"c:\apps\folia\folia.exe"
        );
    }

    // -- debounce --------------------------------------------------------------

    #[test]
    fn publishes_active_after_consecutive_in_use_samples() {
        let mut state = DebounceState::default();
        assert!(!apply_sample_to(&mut state, true)); // sample 1: pending
        assert!(!state.active);
        assert!(apply_sample_to(&mut state, true)); // sample 2: flip
        assert!(state.active);
        assert!(!apply_sample_to(&mut state, true)); // sample 3: stable
        assert!(state.active);
    }

    #[test]
    fn start_requires_two_consecutive_samples() {
        let mut state = DebounceState::default();
        assert!(!apply_sample_to(&mut state, true));
        assert!(!state.active);
        assert!(apply_sample_to(&mut state, true));
        assert!(state.active);
        // 样本方向不变时保持 active
        assert!(!apply_sample_to(&mut state, true));
        assert!(state.active);
    }

    #[test]
    fn stop_requires_three_consecutive_free_samples() {
        let mut state = DebounceState::default();
        let _ = apply_sample_to(&mut state, true);
        let _ = apply_sample_to(&mut state, true);
        assert!(state.active);

        assert!(!apply_sample_to(&mut state, false)); // 1
        assert!(!apply_sample_to(&mut state, false)); // 2
        assert!(state.active); // 仍需第 3 次才暂停
        assert!(apply_sample_to(&mut state, false)); // 3
        assert!(!state.active);
    }

    #[test]
    fn ignores_single_sample_blips() {
        let mut state = DebounceState::default();
        assert!(!apply_sample_to(&mut state, true)); // blip
        assert!(!apply_sample_to(&mut state, false)); // resets instantly
        assert!(!state.active);
        assert!(state.pending_sample.is_none());
        assert_eq!(state.confirm_count, 0);
    }

    #[test]
    fn free_samples_interrupted_by_in_use_reset() {
        let mut state = DebounceState::default();
        let _ = apply_sample_to(&mut state, true);
        let _ = apply_sample_to(&mut state, true);
        assert!(state.active);
        let _ = apply_sample_to(&mut state, false); // 1
        let _ = apply_sample_to(&mut state, false); // 2
        assert!(state.active); // 需要 3 次才暂停
        let _ = apply_sample_to(&mut state, true); // 回到 in-use → 取消 pending
        assert!(state.active);
        assert_eq!(state.pending_sample, None);
        assert_eq!(state.confirm_count, 0);
    }

    // -- monitor (deterministic, no timing) -----------------------------------

    #[test]
    fn disable_releases_active_state_mid_dictation() {
        let enabled = Arc::new(AtomicBool::new(true));
        let enabled_flag = enabled.clone();
        let (monitor, recorded) = VoiceInputPauseMonitor::for_test(
            Arc::new(move || enabled_flag.load(Ordering::SeqCst)),
            Arc::new(|_| Some(true)),
            true,
            1000,
        );

        // 手动驱动两次 in-use 采样（不启动线程，保持确定性）
        monitor.apply_sample(true);
        monitor.apply_sample(true);
        assert_eq!(monitor.get_status()["active"], true);

        enabled.store(false, Ordering::SeqCst);
        let status = monitor.sync_state();
        assert_eq!(status["active"], false);
        assert_eq!(status["enabled"], false);
        assert_eq!(status["supported"], true);
        let last = recorded.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last["active"], false);
    }

    #[test]
    fn unsupported_platform_never_starts_and_reports_false() {
        let (monitor, _) = VoiceInputPauseMonitor::for_test(
            Arc::new(|| true),
            Arc::new(|_| Some(true)),
            false,
            1000,
        );
        let status = monitor.sync_state();
        assert_eq!(status["supported"], false);
        assert_eq!(status["active"], false);
        assert!(monitor.thread.lock().unwrap().is_none());
        monitor.stop();
    }

    // -- regression: recv_timeout 区分 Timeout 与 Disconnected -----------------

    #[test]
    fn recv_timeout_vs_disconnected_distinguished() {
        // 显式 Stop → 退出
        let (tx, rx) = mpsc::channel::<ControlMessage>();
        tx.send(ControlMessage::Stop).unwrap();
        let msg = rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(classify_recv(Ok(msg)), LoopAction::Stop);

        // 发送端消失（stop() 只丢弃 tx 不发消息）→ 退出，而不是当作超时继续采样
        let (tx, rx) = mpsc::channel::<ControlMessage>();
        drop(tx);
        let err = rx.recv_timeout(Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, mpsc::RecvTimeoutError::Disconnected));
        assert_eq!(classify_recv(Err(err)), LoopAction::Stop);

        // 空队列到点 → 照常采样
        let (tx, rx) = mpsc::channel::<ControlMessage>();
        let err = rx.recv_timeout(Duration::from_millis(1)).unwrap_err();
        assert!(matches!(err, mpsc::RecvTimeoutError::Timeout));
        assert_eq!(classify_recv(Err(err)), LoopAction::Tick);
        let _ = tx;
    }

    // -- regression: generation/cancellation 防旧代继续改状态/发事件 ----------

    #[test]
    fn stale_generation_never_mutates_state_or_emits() {
        let inner = Arc::new(Mutex::new(DebounceState::default()));
        let generation = Arc::new(AtomicU64::new(1));
        let recorded: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        let emit = move |payload: &Value| rec.lock().unwrap().push(payload.clone());
        let is_enabled = || true;

        // 当前代（1）采样 in_use=true：第一样本仅 pending，不发事件
        assert!(apply_sampled_state(
            &inner,
            &generation,
            1,
            Some(true),
            &is_enabled,
            true,
            &emit
        ));
        assert_eq!(inner.lock().unwrap().pending_sample, Some(true));
        assert_eq!(inner.lock().unwrap().confirm_count, 1);
        assert!(recorded.lock().unwrap().is_empty());

        // 停止/禁用 → 代递增，旧代（my_gen=1）过期
        generation.fetch_add(1, Ordering::SeqCst);

        // 旧代即使查询结果齐全也绝不触碰状态、绝不发事件，并报告过期
        assert!(!apply_sampled_state(
            &inner,
            &generation,
            1,
            Some(true),
            &is_enabled,
            true,
            &emit
        ));
        assert_eq!(inner.lock().unwrap().pending_sample, Some(true));
        assert_eq!(inner.lock().unwrap().confirm_count, 1);
        assert!(recorded.lock().unwrap().is_empty());

        // 新代（2）可正常续跑：第二样本翻转 active 并发出事件
        assert!(apply_sampled_state(
            &inner,
            &generation,
            2,
            Some(true),
            &is_enabled,
            true,
            &emit
        ));
        assert_eq!(inner.lock().unwrap().active, true);
        let events = recorded.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["active"], true);
        assert_eq!(events[0]["enabled"], true);
        assert_eq!(events[0]["supported"], true);
    }

    #[test]
    fn generation_invalidation_waits_for_in_flight_emit() {
        let inner = Arc::new(Mutex::new(DebounceState::default()));
        let generation = Arc::new(AtomicU64::new(1));

        // First true sample only arms the start debounce.
        assert!(apply_sampled_state(
            &inner,
            &generation,
            1,
            Some(true),
            &|| true,
            true,
            &|_| {}
        ));

        let (emit_started_tx, emit_started_rx) = mpsc::channel();
        let (release_emit_tx, release_emit_rx) = mpsc::channel();
        let apply_inner = inner.clone();
        let apply_generation = generation.clone();
        let apply_thread = std::thread::spawn(move || {
            apply_sampled_state(
                &apply_inner,
                &apply_generation,
                1,
                Some(true),
                &|| true,
                true,
                &|_| {
                    emit_started_tx.send(()).unwrap();
                    release_emit_rx.recv().unwrap();
                },
            )
        });
        emit_started_rx.recv().unwrap();

        let (invalidated_tx, invalidated_rx) = mpsc::channel();
        let invalidation_inner = inner.clone();
        let invalidation_generation = generation.clone();
        let invalidation_thread = std::thread::spawn(move || {
            let _state = invalidation_inner.lock().unwrap();
            invalidation_generation.fetch_add(1, Ordering::SeqCst);
            invalidated_tx.send(()).unwrap();
        });

        assert!(matches!(
            invalidated_rx.recv_timeout(Duration::from_millis(25)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_emit_tx.send(()).unwrap();
        assert!(apply_thread.join().unwrap());
        invalidated_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        invalidation_thread.join().unwrap();
        assert_eq!(generation.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn query_failure_skips_sample_but_keeps_current_generation() {
        let inner = Arc::new(Mutex::new(DebounceState::default()));
        let generation = Arc::new(AtomicU64::new(1));
        let recorded: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        let emit = move |payload: &Value| rec.lock().unwrap().push(payload.clone());
        let is_enabled = || true;

        // 查询失败（None）：跳过采样但线程继续
        assert!(apply_sampled_state(
            &inner,
            &generation,
            1,
            None,
            &is_enabled,
            true,
            &emit
        ));
        assert_eq!(inner.lock().unwrap().pending_sample, None);
        assert!(recorded.lock().unwrap().is_empty());

        // 过期代 + 查询失败 → 仍报告过期
        generation.fetch_add(1, Ordering::SeqCst);
        assert!(!apply_sampled_state(
            &inner,
            &generation,
            1,
            None,
            &is_enabled,
            true,
            &emit
        ));
    }

    #[test]
    fn generation_advances_on_restart_and_stop() {
        let enabled = Arc::new(AtomicBool::new(true));
        let enabled_flag = enabled.clone();
        let (monitor, _) = VoiceInputPauseMonitor::for_test(
            Arc::new(move || enabled_flag.load(Ordering::SeqCst)),
            Arc::new(|_| Some(false)),
            true,
            60000, // 长周期避免测试期间线程自行反复采样
        );
        let gen = monitor.generation.clone();
        assert_eq!(gen.load(Ordering::SeqCst), 0);

        // 启用 → 启动线程并领取新代
        assert!(monitor.sync_state()["enabled"].as_bool().unwrap());
        assert_eq!(gen.load(Ordering::SeqCst), 1);
        assert!(monitor.thread.lock().unwrap().is_some());

        // 关闭 → 立即推进代，残留线程失效
        enabled.store(false, Ordering::SeqCst);
        monitor.sync_state();
        assert_eq!(gen.load(Ordering::SeqCst), 2);
        assert!(monitor.thread.lock().unwrap().is_none());

        // 快速重新启用 → 新线程拿到更新的一代
        enabled.store(true, Ordering::SeqCst);
        assert!(monitor.sync_state()["enabled"].as_bool().unwrap());
        assert_eq!(gen.load(Ordering::SeqCst), 3);

        // stop → 代再推进
        monitor.stop();
        assert_eq!(gen.load(Ordering::SeqCst), 4);
    }

    // -- regression: 并发 stop/sync_state 的锁序（inner → thread → control）--

    /// stop() 必须先取 thread 再取 control（与 spawn_monitor_thread 的
    /// thread → control 一致）。并发地 stop / 启用-停用 / 启动必须都能在
    /// 有限时间内完成，否则（旧实现 control → thread 的逆序在并发下形成
    /// AB-BA 死锁）超时通道会捕获。
    #[test]
    fn concurrent_stop_sync_start_finish_within_timeout() {
        let enabled = Arc::new(AtomicBool::new(true));
        let enabled_flag = enabled.clone();
        let (monitor, _) = VoiceInputPauseMonitor::for_test(
            Arc::new(move || enabled_flag.load(Ordering::SeqCst)),
            Arc::new(|_| Some(false)),
            true,
            60_000, // 长周期：避免测试期间后台线程自行反复采样
        );
        let monitor = Arc::new(monitor);

        let (done_tx, done_rx) = mpsc::channel::<()>();
        for worker in 0..4 {
            let monitor = monitor.clone();
            let enabled = enabled.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                for i in 0..25 {
                    match (worker + i) % 3 {
                        0 => {
                            enabled.store(true, Ordering::SeqCst);
                            let _ = monitor.sync_state();
                        }
                        1 => {
                            enabled.store(false, Ordering::SeqCst);
                            let _ = monitor.sync_state();
                        }
                        _ => monitor.stop(),
                    }
                }
                monitor.stop();
                let _ = done_tx.send(());
            });
        }
        drop(done_tx);
        // 若锁序不一致导致死锁，下面的 recv_timeout 会超时（证明不死锁）。
        for _ in 0..4 {
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("deadlock: stop/sync_state 未在 10s 内完成");
        }
        // 收尾：残留线程由代过期/Stop 退出；stop 幂等，且自身也必须在限时内完成。
        monitor.stop();
    }

    // -- regression: reg query stdout 并发排空（Windows） ---------------------

    #[test]
    #[cfg(windows)]
    fn large_child_output_is_drained_without_timeout() {
        // `reg query ... /s` 的输出可能远超匿名 pipe 缓冲（~64KB）。
        // 旧实现先 wait 再读 stdout，写满的 pipe 会把子进程卡到 5s 超时 → None；
        // 新实现并发排空，应完整读回全部输出。
        let output = run_with_drain(
            {
                let mut cmd = std::process::Command::new("cmd");
                cmd.args([
                    "/d",
                    "/c",
                    "for /l %i in (1,1,30000) do @echo HKEY_CURRENT_USER\\some\\key",
                ]);
                cmd
            },
            Duration::from_secs(5),
        );
        let output = output.expect("large output must be drained, not timeout-killed");
        assert_eq!(output.lines().count(), 30_000);
    }

    /// 手动冒烟：`FOLIA_SMOKE=1 cargo test` 时真实执行 `reg query` 麦克风
    /// ConsentStore 探测（Windows）。失败/无数据时返回 None（诚实降级）。
    #[test]
    #[cfg(windows)]
    fn smoke_microphone_probe() {
        if std::env::var("FOLIA_SMOKE").as_deref() != Ok("1") {
            eprintln!("SMOKE(voice): skipped (set FOLIA_SMOKE=1 to run)");
            return;
        }
        let own_exe = std::env::current_exe()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        match query_windows_microphone_in_use(&own_exe) {
            Some(in_use) => eprintln!("SMOKE(voice): microphone probe -> in_use={in_use}"),
            None => eprintln!("SMOKE(voice): reg query unavailable/failed"),
        }
    }
}
