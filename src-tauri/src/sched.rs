//! 后台同步调度线程（2026-09-10 用户反馈后调整：编辑不再即时同步，太慢）：
//!
//! | 事件 | 动作 |
//! |---|---|
//! | 每 30min tick | 有本地改动 → 完整同步（自动保存）；无 → 静默 pull |
//! | 同步失败（非冲突） | 30s 后重试（PRD：恢复后 ≤60s 补推） |
//! | 手动同步（F20，点角标） | 立即完整同步 |
//! | 退出（FlushNow） | 最后一次完整同步后收线程 |
//!
//! 编辑只落工作区 + pending_edits 计数，由 tick / 手动 / 退出统一推走。
//! 冲突不自动重试（push 被拒的 rebase 重试已内含在 sync_cycle；真冲突需人工处理）。
//! 全部 git 周期持 store 的 io 锁，与编辑的读-改-写互斥（v1 正确性优先，秒级可接受）。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Local;

use crate::store::{StickyStore, SyncUiState, repo_url};
use crate::sync::{commit_prefix, SyncError};

/// 调度消息。SyncNow 由命令层投递（点角标），FlushNow 仅退出流程使用。
pub enum SchedMsg {
    SyncNow,
    FlushNow,
}

#[derive(Debug, Clone)]
pub struct SchedTiming {
    /// tick 周期：有本地改动时走完整同步（= 自动保存），否则静默 pull
    pub tick: Duration,
    /// 失败重试间隔
    pub retry: Duration,
    /// 轮询粒度
    pub poll: Duration,
}

impl Default for SchedTiming {
    fn default() -> Self {
        SchedTiming {
            tick: Duration::from_secs(1800),
            retry: Duration::from_secs(30),
            poll: Duration::from_millis(500),
        }
    }
}

pub struct SchedulerHandle {
    tx: Sender<SchedMsg>,
    done: Receiver<()>,
    _join: JoinHandle<()>,
}

impl SchedulerHandle {
    /// 退出 flush：触发最后一次完整同步并等待完成（超时放弃——本地 commit 已保底）。
    pub fn stop(self, timeout: Duration) -> bool {
        let _ = self.tx.send(SchedMsg::FlushNow);
        let ok = self.done.recv_timeout(timeout).is_ok();
        if !ok {
            log_err!("[sched] 退出 flush 超时（{timeout:?}）：未推送的本地提交已保底，下次启动补推");
        }
        ok
    }
}

/// 事件广播：Tauri 下是 `app.emit(event, payload)`；测试下是收集器。payload 为 JSON 字符串。
pub type EmitFn = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// 启动调度线程。返回句柄供退出 flush；store.attach_scheduler 在内部完成。
pub fn spawn(store: Arc<StickyStore>, timing: SchedTiming, emit: EmitFn) -> SchedulerHandle {
    spawn_with_repo_url(store, timing, emit, repo_url())
}

/// 显式目标仓库，供隔离环境使用，避免测试修改进程级环境变量。
pub fn spawn_with_repo_url(store: Arc<StickyStore>, timing: SchedTiming, emit: EmitFn, url: String) -> SchedulerHandle {
    store.mark_repo_not_ready();
    let (tx, rx) = channel::<SchedMsg>();
    let (done_tx, done_rx) = channel::<()>();
    store.attach_scheduler(tx.clone());

    let join = std::thread::Builder::new()
        .name("sticky-sched".into())
        .spawn(move || {
            run_loop(&store, &timing, &emit, &rx, &url);
            let _ = done_tx.send(());
        })
        .expect("调度线程启动失败");

    SchedulerHandle { tx, done: done_rx, _join: join }
}

fn today() -> chrono::NaiveDate {
    Local::now().date_naive()
}

fn emit_status(store: &StickyStore, emit: &EmitFn) {
    let s = store.status();
    if let Ok(p) = serde_json::to_string(&s) {
        emit("sync-status", &p);
    }
}

pub(crate) fn sanitize_sync_error(message: &str, pat: Option<&str>) -> String {
    let mut clean = message.to_owned();
    if let Some(pat) = pat.filter(|pat| !pat.is_empty()) {
        clean = clean.replace(pat, "[redacted]");
    }
    // Strip URL userinfo before truncation, including non-GitHub credentials.
    for scheme in ["https://", "http://"] {
        let mut cursor = 0;
        while let Some(offset) = clean[cursor..].to_ascii_lowercase().find(scheme) {
            let start = cursor + offset + scheme.len();
            let end = clean[start..].find(|c: char| c.is_whitespace() || "/?#\"'<>".contains(c))
                .map_or(clean.len(), |offset| start + offset);
            if let Some(at) = clean[start..end].rfind('@') {
                clean.replace_range(start..start + at + 1, "");
            }
            cursor = start;
        }
    }
    // Recognize GitHub token fragments even when the stored PAT is unavailable.
    for prefix in ["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        while let Some(start) = clean.to_ascii_lowercase().find(prefix) {
            let end = clean[start..].find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .map_or(clean.len(), |offset| start + offset);
            clean.replace_range(start..end, "[redacted]");
        }
    }
    for label in ["bearer", "token", "pat", "password", "authorization"] {
        let mut cursor = 0;
        while let Some(offset) = clean[cursor..].to_ascii_lowercase().find(label) {
            let start = cursor + offset;
            cursor = start + label.len();
            if start > 0 && clean[..start].chars().next_back().is_some_and(|c| c.is_alphanumeric()) {
                continue;
            }
            let tail = &clean[cursor..];
            let value = tail.trim_start_matches(|c: char| c.is_whitespace() || "\"'".contains(c));
            let value = if let Some(value) = value.strip_prefix('=').or_else(|| value.strip_prefix(':')) {
                value.trim_start_matches(|c: char| c.is_whitespace() || "\"'".contains(c))
            } else if label == "bearer" && tail.starts_with(char::is_whitespace) {
                value
            } else {
                continue;
            };
            let value_start = clean.len() - value.len();
            let length = value.find(|c: char| c.is_whitespace() || "\"',;&)]}".contains(c))
                .unwrap_or(value.len());
            if length > 0 && !value.starts_with("[redacted]") {
                clean.replace_range(value_start..value_start + length, "[redacted]");
                cursor = value_start + "[redacted]".len();
            }
        }
    }
    clean.chars().take(200).collect()
}

pub(crate) fn sync_error_message(error: &SyncError) -> String {
    #[cfg(target_os = "android")]
    let pat = crate::sync_config::pat();
    #[cfg(not(target_os = "android"))]
    let pat: Option<String> = None;
    format!("同步失败: {}", sanitize_sync_error(&error.to_string(), pat.as_deref()))
}

/// 启动时确保仓库可用。失败 → Error 状态 + 按 retry 重试。
fn ensure_repo(store: &StickyStore, retry_at: &mut Option<Instant>, timing: &SchedTiming, emit: &EmitFn, url: &str) -> bool {
    // 克隆全程持 io 锁：命令层的读-改-写不与 checkout 交叠（.git 中途就会出现，
    // repo_ready 旗标此时仍为 false，命令一律 repo_not_ready 挡住）
    let cloned = {
        let _g = store.io_lock();
        store.engine().ensure_cloned(url)
    };
    let ready = cloned.is_ok();
    match cloned {
        Ok(_) => {
            *retry_at = None;
            store.mark_repo_ready();
            store.set_status(SyncUiState::Idle);
            emit("repo-ready", "{}");
        }
        Err(e) => {
            let message = sync_error_message(&e);
            log_err!("[sched] 仓库校验/克隆失败，{:?} 后重试: {message}", timing.retry);
            *retry_at = Some(Instant::now() + timing.retry);
            store.set_status(SyncUiState::Error { message });
        }
    }
    if !ready {
        emit_status(store, emit);
    }
    ready
}

fn run_loop(store: &Arc<StickyStore>, timing: &SchedTiming, emit: &EmitFn, rx: &Receiver<SchedMsg>, url: &str) {
    let mut last_tick = Instant::now();
    let mut retry_at: Option<Instant> = None;

    let mut ready = ensure_repo(store, &mut retry_at, timing, emit, url);
    // 启动即同步：隔夜远端变更不等 30min tick（避免在陈旧基线上编辑触发可避免的
    // rebase 冲突），顺带补推上次退出未推完的积压。
    if ready {
        full_sync(store, timing, &mut retry_at, emit);
    }
    // 跨日流转（Win 专属）：先同步拿最新基线再搬；有搬运立即补一轮同步推走，
    // 其余端尽快看到今天的合并结果。随后的 view-changed 让前端首拉即终态。
    #[cfg(windows)]
    if ready {
        rollover_and_maybe_sync(store, timing, &mut retry_at, emit);
    }
    // 首次就绪后广播一次，前端据此拉首个视图
    emit("view-changed", "[]");

    #[cfg(windows)]
    let mut last_date = today();
    loop {
        let (manual, flush) = match rx.recv_timeout(timing.poll) {
            Ok(SchedMsg::SyncNow) => (true, false),
            Ok(SchedMsg::FlushNow) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => (true, true),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (false, false),
        };
        let now = Instant::now();
        let tick = now.duration_since(last_tick) >= timing.tick;
        let retry = retry_at.is_some_and(|at| now >= at);
        if tick {
            last_tick = now;
        }
        if manual || tick || retry {
            let recovered = !ready;
            if !ready {
                ready = ensure_repo(store, &mut retry_at, timing, emit, url);
            }
            // 校验失败时，手动、tick、重试和退出都不能绕过仓库门禁。
            if ready {
                if manual || recovered || retry_at.is_some() || store.engine().has_local_changes() {
                    full_sync(store, timing, &mut retry_at, emit);
                } else {
                    silent_pull(store, &mut retry_at, emit);
                }
            }
        }
        // 跨午夜检测（Win 专属）：常驻窗口翻日。搬运后立即推走；无论是否
        // 有搬运都广播 view-changed，前端刷新日期标签与今日视图。
        #[cfg(windows)]
        if ready {
            let now_date = today();
            if now_date != last_date {
                last_date = now_date;
                rollover_and_maybe_sync(store, timing, &mut retry_at, emit);
                emit("view-changed", "[]");
            }
        }
        if flush {
            break;
        }
    }
}

/// 执行跨日流转；有搬运则立即完整同步推走（Win 专属）。返回是否有文件变化。
#[cfg(windows)]
fn rollover_and_maybe_sync(
    store: &Arc<StickyStore>,
    timing: &SchedTiming,
    retry_at: &mut Option<Instant>,
    emit: &EmitFn,
) -> bool {
    match crate::rollover::carry_over(store, today()) {
        Ok(report) if report.moved + report.merged > 0 => {
            log_err!("[sched] 跨日流转: {} 项搬入今天, {} 项并入同文", report.moved, report.merged);
            full_sync(store, timing, retry_at, emit);
            true
        }
        // 本轮没搬不等于没事：启动/跨午夜前最后一轮同步的补扫可能已把历史
        // 未完成项搬入（补扫刻意只记数不推送）。凭 pending 补一轮同步，
        // 兑现「有搬运立即推走」，否则结果要滞留到下个 30 分钟 tick。
        Ok(_) if store.pending_edit_count() > 0 => {
            log_err!("[sched] 补扫已有搬运积压，补一轮同步推走");
            full_sync(store, timing, retry_at, emit);
            true
        }
        Ok(_) => false,
        Err(e) => {
            log_err!("[sched] 跨日流转失败（下个周期重试）: {}", e.message);
            false
        }
    }
}

/// 同步成功后的补扫（Win 专属）：远端（如安卓 F14 复制、他机编辑）可能带入
/// 新的历史未完成项。有搬运只发事件不发同步——推走交给下个 tick。
#[cfg(windows)]
fn rollover_rescan(store: &Arc<StickyStore>, emit: &EmitFn) {
    match crate::rollover::carry_over(store, today()) {
        Ok(report) if report.moved + report.merged > 0 => {
            log_err!("[sched] 同步后补扫搬入 {} 项", report.moved + report.merged);
            if let Ok(p) = serde_json::to_string(&store.watched_fingerprints(today())) {
                emit("view-changed", &p);
            }
        }
        Ok(_) => {}
        Err(e) => log_err!("[sched] 同步后补扫失败（下个周期重试）: {}", e.message),
    }
}

fn commit_message(pending: usize) -> String {
    let d = today().format("%Y-%m-%d");
    // PRD 形如 `sticky@office: 2026-09-10 add 3 / done 2 (day, week)`——v1 汇总为编辑计数
    format!("{} {d} {pending} edits", commit_prefix())
}

fn full_sync(store: &Arc<StickyStore>, timing: &SchedTiming, retry_at: &mut Option<Instant>, emit: &EmitFn) {
    let before = store.watched_fingerprints(today());
    let pending = store.take_pending_edits();
    let msg = commit_message(pending);

    store.set_status(SyncUiState::Syncing);
    emit_status(store, emit);

    let result = store.engine().sync_cycle_staged(&msg, store.io_mutex());

    match result {
        Ok(_) => {
            *retry_at = None;
            store.set_status(SyncUiState::Idle);
            #[cfg(windows)]
            rollover_rescan(store, emit);
        }
        Err(SyncError::Conflict(m)) => {
            // 真冲突：本地提交保留，等人工处理（重试按钮 / 打开仓库文件夹）
            *retry_at = None;
            let message = sync_error_message(&SyncError::Conflict(m));
            #[cfg(target_os = "android")]
            log::error!(target: "sticky", "[sched] {message}");
            store.set_status(SyncUiState::Conflict { message });
        }
        Err(e) => {
            let message = sync_error_message(&e);
            log_err!("[sched] 同步失败，{:?} 后重试: {message}", timing.retry);
            *retry_at = Some(Instant::now() + timing.retry);
            store.set_status(SyncUiState::Error { message });
        }
    }
    emit_status(store, emit);

    let after = store.watched_fingerprints(today());
    if after != before {
        if let Ok(p) = serde_json::to_string(&after) {
            emit("view-changed", &p);
        }
    }
}

fn silent_pull(store: &Arc<StickyStore>, retry_at: &mut Option<Instant>, emit: &EmitFn) {
    let before = store.watched_fingerprints(today());
    let result = store.engine().pull_staged(store.io_mutex());
    match result {
        Ok(()) => {
            #[cfg(windows)]
            rollover_rescan(store, emit);
        }
        Err(SyncError::Conflict(m)) => {
            *retry_at = None;
            let message = sync_error_message(&SyncError::Conflict(m));
            #[cfg(target_os = "android")]
            log::error!(target: "sticky", "[sched] {message}");
            store.set_status(SyncUiState::Conflict { message });
            emit_status(store, emit);
        }
        Err(e) => {
            // 静默 pull 失败：本地干净则不打扰（真离线）；有积压才亮角标
            let message = sync_error_message(&e);
            log_err!("[sched] 静默 pull 失败: {message}");
            let unpushed = store.engine().unpushed_count().unwrap_or(0);
            if store.engine().has_local_changes() || unpushed > 0 {
                *retry_at = Some(Instant::now() + Duration::from_secs(30));
                store.set_status(SyncUiState::Error { message });
                emit_status(store, emit);
            }
        }
    }
    let after = store.watched_fingerprints(today());
    if after != before {
        if let Ok(p) = serde_json::to_string(&after) {
            emit("view-changed", &p);
        }
    }
}

#[cfg(test)]
mod error_message_tests {
    use super::sanitize_sync_error;

    #[test]
    fn sanitize_sync_error_preserves_safe_messages_and_is_idempotent() {
        for message in ["", "certificate verify failed", "网络失败，PAT 权限不足"] {
            assert_eq!(sanitize_sync_error(message, Some("")), message);
            assert_eq!(sanitize_sync_error(&sanitize_sync_error(message, None), None), message);
        }
        let clean = sanitize_sync_error(&"证".repeat(250), None);
        assert_eq!(clean.chars().count(), 200);
        assert_eq!(sanitize_sync_error(&clean, None), clean);
    }

    #[test]
    fn sanitize_sync_error_removes_credentials_before_truncation() {
        let message = "TLS https://alice:secret@example.com/repo HTTPS://bob:pass@example.org/repo \
            ghp_fragment github_pat_fragment token=opaque PAT: 'private' \
            Authorization: Bearer bearer-secret stored-secret";
        let clean = sanitize_sync_error(message, Some("stored-secret"));
        for secret in ["alice", "secret", "bob", "pass@", "fragment", "opaque", "private"] {
            assert!(!clean.contains(secret), "leaked {secret}: {clean}");
        }
        assert!(clean.contains("https://example.com/repo"));
        assert_eq!(sanitize_sync_error(&clean, Some("stored-secret")), clean);
        let long_url = format!("https://user:{}@example.com TLS failed", "x".repeat(250));
        assert_eq!(sanitize_sync_error(&long_url, None), "https://example.com TLS failed");
        assert_eq!(sanitize_sync_error("Authorization: Bearer opaque", None), "Authorization: [redacted] [redacted]");
    }
}
