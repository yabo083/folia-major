// src-tauri/src/handoff.rs
// M2：windowPlaybackHandoff.cjs 的 Tauri 移植。渲染进程在透明窗口重建期间的播放状态交接，
// 短时存储（TTL 15s、consume 一次即清、过期清空），并维护 requestId 挂起请求表
// （pendingWindowPlaybackHandoffRequests 语义，供 M7 遥控透明切换使用）。

use serde_json::Value;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

const DEFAULT_TTL_MS: u128 = 15_000;

fn system_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn is_handoff_object(value: &Value) -> bool {
    value.is_object()
}

#[derive(Default)]
struct StoreInner {
    current: Option<(Value, u128)>,
    pending: HashMap<String, (Sender<Value>, u128)>,
}

pub struct WindowPlaybackHandoffStore {
    inner: Mutex<StoreInner>,
    ttl_ms: u128,
    clock: Box<dyn Fn() -> u128 + Send + Sync>,
}

impl WindowPlaybackHandoffStore {
    pub fn new() -> Self {
        Self::with_clock(DEFAULT_TTL_MS, Box::new(system_now_ms))
    }

    // 测试用：注入 ttl 与时钟（对应 JS 的 createWindowPlaybackHandoffStore options）。
    pub fn with_clock(ttl_ms: u128, clock: Box<dyn Fn() -> u128 + Send + Sync>) -> Self {
        Self {
            inner: Mutex::new(StoreInner::default()),
            ttl_ms,
            clock,
        }
    }

    fn lock(&self) -> MutexGuard<'_, StoreInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // 保存交接载荷；非法（非对象/null）载荷清空当前交接并返回 false。
    pub fn save(&self, handoff: Value) -> bool {
        if !is_handoff_object(&handoff) {
            self.lock().current = None;
            return false;
        }
        let expires_at = (self.clock)() + self.ttl_ms;
        self.lock().current = Some((handoff, expires_at));
        true
    }

    // 消费一次交接；未保存或已过期返回 None 并清空。
    pub fn consume(&self) -> Option<Value> {
        let now = (self.clock)();
        let mut inner = self.lock();
        match &inner.current {
            Some((_, expires_at)) if now <= *expires_at => inner.current.take().map(|(h, _)| h),
            _ => {
                inner.current = None;
                None
            }
        }
    }

    // 查看当前交接；过期自动清空。
    #[allow(dead_code)] // M7 遥控透明切换会使用；当前仅测试覆盖
    pub fn peek(&self) -> Option<Value> {
        let now = (self.clock)();
        let mut inner = self.lock();
        match &inner.current {
            Some((handoff, expires_at)) if now <= *expires_at => Some(handoff.clone()),
            _ => {
                inner.current = None;
                None
            }
        }
    }

    #[allow(dead_code)] // 对齐 windowPlaybackHandoff.cjs 的 clear 契约
    pub fn clear(&self) {
        self.lock().current = None;
    }

    // 注册挂起请求，返回接收端；超时后由 resolve 返回 false。
    #[allow(dead_code)] // M7 的 requestWindowPlaybackHandoff 使用
    pub fn register(&self, request_id: String, timeout_ms: u128) -> Receiver<Value> {
        let now = (self.clock)();
        let (tx, rx) = channel();
        self.lock()
            .pending
            .insert(request_id, (tx, now + timeout_ms));
        rx
    }

    // 解析挂起请求（先由调用方负责 save）；成功发送 handoff 并返回 true，未找到或已过期返回 false。
    pub fn resolve(&self, request_id: &str, handoff: Option<Value>) -> bool {
        let now = (self.clock)();
        let mut inner = self.lock();
        match inner.pending.remove(request_id) {
            Some((tx, expires_at)) if now <= expires_at => {
                let _ = tx.send(handoff.unwrap_or(Value::Null));
                true
            }
            Some(_) | None => false,
        }
    }

    // 清理全部挂起请求并以 null 解析。
    pub fn clear_all(&self) {
        let mut inner = self.lock();
        for (_, (tx, _)) in inner.pending.drain() {
            let _ = tx.send(Value::Null);
        }
    }

    // 清理已过期的挂起请求。
    #[allow(dead_code)] // M7 的定时清理使用
    pub fn clear_expired(&self) {
        let now = (self.clock)();
        self.lock()
            .pending
            .retain(|_, (_, expires_at)| now <= *expires_at);
    }
}

impl Default for WindowPlaybackHandoffStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn fake_clock(now: Arc<AtomicU64>) -> Box<dyn Fn() -> u128 + Send + Sync> {
        Box::new(move || now.load(Ordering::Relaxed) as u128)
    }

    #[test]
    fn consumes_a_saved_handoff_only_once() {
        let now = Arc::new(AtomicU64::new(1000));
        let store = WindowPlaybackHandoffStore::with_clock(5000, fake_clock(now.clone()));
        let handoff = json!({ "version": 1, "capturedAt": now.load(Ordering::Relaxed) });

        assert!(store.save(handoff.clone()));
        assert_eq!(store.peek(), Some(handoff.clone()));
        assert_eq!(store.consume(), Some(handoff));
        assert_eq!(store.consume(), None);
    }

    #[test]
    fn drops_expired_handoffs() {
        let now = Arc::new(AtomicU64::new(1000));
        let store = WindowPlaybackHandoffStore::with_clock(100, fake_clock(now.clone()));

        store.save(json!({ "version": 1, "capturedAt": now.load(Ordering::Relaxed) }));
        now.store(1101, Ordering::Relaxed);

        assert_eq!(store.peek(), None);
        assert_eq!(store.consume(), None);
    }

    #[test]
    fn clears_current_handoff_when_saving_invalid_payloads() {
        let store = WindowPlaybackHandoffStore::new();

        assert!(store.save(json!({ "version": 1, "capturedAt": 1 })));
        assert!(!store.save(Value::Null));
        assert!(!store.save(json!([1, 2, 3])));
        assert_eq!(store.consume(), None);
    }

    #[test]
    fn resolves_a_pending_request_once() {
        let store = WindowPlaybackHandoffStore::new();
        let rx = store.register("req-1".into(), 800);
        let handoff = json!({ "version": 1, "capturedAt": 1 });

        assert!(store.resolve("req-1", Some(handoff.clone())));
        assert_eq!(rx.recv().unwrap(), handoff);
        assert!(!store.resolve("req-1", None));
    }

    #[test]
    fn rejects_expired_pending_requests() {
        let now = Arc::new(AtomicU64::new(1000));
        let store = WindowPlaybackHandoffStore::with_clock(800, fake_clock(now.clone()));

        let _rx = store.register("req-1".into(), 800);
        now.store(2000, Ordering::Relaxed);

        assert!(!store.resolve("req-1", Some(json!({}))));
    }

    #[test]
    fn clears_all_pending_requests_with_null() {
        let store = WindowPlaybackHandoffStore::new();
        let rx = store.register("req-1".into(), 800);

        store.clear_all();

        assert_eq!(rx.recv().unwrap(), Value::Null);
        assert!(!store.resolve("req-1", None));
    }
}
