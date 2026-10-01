//! git CLI 同步引擎。
//!
//! 同步周期 commit → pull --rebase → push（编辑由调用方在周期之间完成；
//! record.py 是编辑前单次 pull，本引擎面对常有未提交改动的桌面场景，
//! 必须先 commit 再 rebase，否则 "unstaged changes" 会卡死重试）。
//!
//! 边界（仓库 CLAUDE.md）：
//! - widget 是第三写入方；绝不执行外部例行任务职责、绝不写 `_current.md`
//! - commit 前缀 `sticky@<machine>:`（每机身份，见 machine_tag()）
//! - 所有 git 子进程禁用交互提示（GIT_TERMINAL_PROMPT=0），凭据走系统凭据管理器

use std::fmt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::merge_resolve::ConflictSides;

#[cfg(any(target_os = "android", test))]
#[path = "sync_gix.rs"]
mod sync_gix;

/// 每机提交身份（多终端同仓同步，见 docs/多机安装.md）：
/// `STICKY_MACHINE_TAG` 环境变量 → `%APPDATA%\sticky-todo\machine.txt` →
/// 计算机名小写（截 12 字符）。纯函数便于测试。
fn resolve_machine_tag(env_tag: Option<&str>, file_tag: Option<&str>, hostname: &str) -> String {
    let clean = |s: &str| s.trim().trim_start_matches('\u{feff}').trim().to_string();
    if let Some(t) = env_tag.map(clean).filter(|t| !t.is_empty()) {
        return t;
    }
    if let Some(t) = file_tag.map(clean).filter(|t| !t.is_empty()) {
        return t;
    }
    hostname.to_lowercase().chars().take(12).collect()
}

pub fn machine_tag() -> String {
    let file_tag = std::env::var_os("APPDATA").and_then(|appdata| {
        std::fs::read_to_string(
            PathBuf::from(appdata).join("sticky-todo").join("machine.txt"),
        )
        .ok()
    });
    resolve_machine_tag(
        std::env::var("STICKY_MACHINE_TAG").ok().as_deref(),
        file_tag.as_deref(),
        &std::env::var("COMPUTERNAME").unwrap_or_default(),
    )
}

/// commit 前缀形如 `sticky@<machine>:`；tag 部分每机不同（可区分写入方）。
pub fn commit_prefix() -> String {
    format!("sticky@{}:", machine_tag())
}

/// 一次同步周期的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    /// 本地无待推送提交，仅完成拉取
    NothingToPush,
    /// 提交并推送成功；rebased = 是否经历了一次 push 被拒后的 rebase 重试；
    /// auto_merged = 本周期语义自动合并写回的文件（空 = 无自动合并发生）
    Pushed { rebased: bool, auto_merged: Vec<String> },
}

#[derive(Debug)]
pub enum SyncError {
    /// git 不存在 / 无法执行
    GitUnavailable(String),
    /// git 返回非零且非冲突语义
    Git(String),
    /// rebase 冲突（已 abort，仓库回到干净状态，本地提交保留待处理）
    Conflict(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::GitUnavailable(s) => write!(f, "无法执行 git: {s}"),
            SyncError::Git(s) => write!(f, "git 失败: {s}"),
            SyncError::Conflict(s) => write!(f, "同步冲突: {s}"),
        }
    }
}

impl std::error::Error for SyncError {}

/// git 后端原语。桌面走系统 git CLI（CliGitBackend，本文件）；
/// 安卓无 git CLI，由 gix + rustls 实现；host 单元测试也使用同一后端。
pub trait GitBackend: Send + Sync {
    /// 分阶段同步仅由桌面 CLI 启用；其他后端保留融合流程。
    fn supports_staged_sync(&self) -> bool { false }
    /// 纯网络段，不触碰工作区。
    fn fetch(&self, _repo_dir: &Path) -> Result<(), SyncError> { Ok(()) }
    /// 工作区段；默认仍调用融合版 pull/rebase。
    fn rebase(&self, repo_dir: &Path) -> Result<(), SyncError> { self.pull_rebase(repo_dir) }
    /// 首次克隆并校验 origin 一致性。返回是否新建克隆。
    fn ensure_cloned(&self, repo_dir: &Path, url: &str) -> Result<bool, SyncError>;
    /// `git pull --rebase`；冲突时自动 abort 并返回 Conflict。
    fn pull_rebase(&self, repo_dir: &Path) -> Result<(), SyncError>;
    /// add -A + commit（身份先补齐）。无改动返回 Ok(false)。
    fn commit_all(&self, repo_dir: &Path, message: &str) -> Result<bool, SyncError>;
    /// 裸 push（被拒即 Err，不重试；调用方决定是否走 push_with_retry）。
    fn push(&self, repo_dir: &Path) -> Result<(), SyncError>;
    /// push 被拒（远端有新提交）→ rebase 后重试一次。返回是否经历了 rebase。
    fn push_with_retry(&self, repo_dir: &Path) -> Result<bool, SyncError>;
    /// 领先远端的本地提交数（离线队列深度）。
    fn unpushed_count(&self, repo_dir: &Path) -> Result<usize, SyncError>;
    /// 工作区是否有未提交改动（调度器 tick 判断走静默 pull 还是完整同步）。
    fn has_local_changes(&self, repo_dir: &Path) -> bool;
    /// 清理前次同步中断留下的 merge/rebase 半程态（崩溃恢复）。
    /// 清理失败（半程态残留）必须返回 Err：此时继续同步会把带冲突标记的
    /// 工作区文本 `add -A` 提交并推走，宁可直接进人工冲突态。
    fn recover_interrupted(&self, _repo_dir: &Path) -> Result<(), SyncError> { Ok(()) }
    /// 取走（并清空）最近一次语义自动合并的文件清单。
    fn take_auto_merged(&self) -> Vec<String> { Vec::new() }
}

// Log only sanitized messages at engine boundaries; backend internals remain unchanged.
#[cfg(target_os = "android")]
fn log_stage_error<T>(stage: &str, result: &Result<T, SyncError>) {
    if let Err(error) = result {
        let message = crate::sched::sync_error_message(error);
        log::error!(target: "sticky", "[sync] {stage} failed: {message}");
    }
}

pub struct SyncEngine {
    repo_dir: PathBuf,
    backend: Box<dyn GitBackend>,
}

impl SyncEngine {
    pub fn new(repo_dir: impl Into<PathBuf>) -> Self {
        #[cfg(target_os = "android")]
        let backend: Box<dyn GitBackend> = Box::new(sync_gix::GixBackend::new(
            std::sync::Arc::new(|| {
                let pat = crate::sync_config::pat();
                log::info!(target: "sticky", "[sync] PAT provider some={}", pat.is_some());
                pat
            }),
        ));
        #[cfg(all(test, not(target_os = "android")))]
        let backend: Box<dyn GitBackend> = Box::new(sync_gix::GixBackend::new(
            std::sync::Arc::new(|| None),
        ));
        #[cfg(not(any(target_os = "android", test)))]
        let backend: Box<dyn GitBackend> = Box::new(CliGitBackend::default());
        SyncEngine::with_backend(repo_dir, backend)
    }

    /// 注入自定义后端（非桌面平台 / 测试）。
    pub fn with_backend(repo_dir: impl Into<PathBuf>, backend: Box<dyn GitBackend>) -> Self {
        SyncEngine { repo_dir: repo_dir.into(), backend }
    }

    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }

    /// 目录已是 git 仓库 → Ok(false)；不存在 → 从 url 克隆 → Ok(true)。
    pub fn ensure_cloned(&self, url: &str) -> Result<bool, SyncError> {
        #[cfg(target_os = "android")]
        let host = {
            let parsed = tauri::Url::parse(url).ok();
            crate::sched::sanitize_sync_error(
                parsed.as_ref().and_then(|url| url.host_str()).unwrap_or("unknown"),
                crate::sync_config::pat().as_deref(),
            )
        };
        #[cfg(target_os = "android")]
        log::info!(target: "sticky", "[sync] ensure_cloned start host={host}");
        let result = self.backend.ensure_cloned(&self.repo_dir, url);
        #[cfg(target_os = "android")]
        {
            log::info!(target: "sticky", "[sync] ensure_cloned end host={host} ok={} cloned={}",
                result.is_ok(), matches!(&result, Ok(true)));
            log_stage_error("clone/validate (includes fetch)", &result);
        }
        result
    }

    /// `git pull --rebase`。冲突时自动 abort 并返回 Conflict。
    pub fn pull(&self) -> Result<(), SyncError> {
        let result = self.backend.pull_rebase(&self.repo_dir);
        #[cfg(target_os = "android")]
        log_stage_error("fetch/rebase", &result);
        result
    }

    /// add -A + commit。无改动返回 Ok(false)。
    pub fn commit_all(&self, message: &str) -> Result<bool, SyncError> {
        let result = self.backend.commit_all(&self.repo_dir, message);
        #[cfg(target_os = "android")]
        log_stage_error("commit", &result);
        result
    }

    /// 裸 push（被拒即 Err；调度器/测试用）。
    pub fn push(&self) -> Result<(), SyncError> {
        let result = self.backend.push(&self.repo_dir);
        #[cfg(target_os = "android")]
        log_stage_error("push", &result);
        result
    }

    /// push 被拒（远端有新提交，如外部任务例行推送）→ rebase 后重试一次。
    /// 返回是否经历了 rebase。
    pub fn push_with_retry(&self) -> Result<bool, SyncError> {
        let result = self.backend.push_with_retry(&self.repo_dir);
        #[cfg(target_os = "android")]
        log_stage_error("push/retry (includes fetch/rebase)", &result);
        result
    }

    /// 领先远端的本地提交数（离线队列深度）。
    pub fn unpushed_count(&self) -> Result<usize, SyncError> {
        let result = self.backend.unpushed_count(&self.repo_dir);
        #[cfg(target_os = "android")]
        log_stage_error("unpushed_count", &result);
        result
    }

    /// 工作区是否有未提交改动（调度器 tick 判断走静默 pull 还是完整同步）。
    pub fn has_local_changes(&self) -> bool {
        self.backend.has_local_changes(&self.repo_dir)
    }

    /// 完整周期：commit → pull → push（顺序即正确性：工作区常有未提交的
    /// tracked 改动，先 pull --rebase 会因 "unstaged changes" exit 128 死循环；
    /// 先 commit 后 rebase 才能安全收敛多端历史）。离线积压的本地提交也会被推送。
    pub fn sync_cycle(&self, message: &str) -> Result<SyncStatus, SyncError> {
        self.backend.recover_interrupted(&self.repo_dir)?;
        let committed = self.commit_all(message)?;
        self.pull()?;
        if !committed && self.unpushed_count()? == 0 {
            return Ok(SyncStatus::NothingToPush);
        }
        let rebased = self.push_with_retry()?;
        let auto_merged = self.backend.take_auto_merged();
        Ok(SyncStatus::Pushed { rebased, auto_merged })
    }
}

impl SyncEngine {
    /// commit/rebase 持有 io 锁，fetch/push 释放锁以允许桌面命令继续执行。
    pub fn sync_cycle_staged(&self, message: &str, io: &std::sync::Mutex<()>) -> Result<SyncStatus, SyncError> {
        if !self.backend.supports_staged_sync() {
            let _g = io.lock().unwrap_or_else(|p| p.into_inner());
            return self.sync_cycle(message);
        }
        self.backend.recover_interrupted(&self.repo_dir)?;
        let committed = {
            let _g = io.lock().unwrap_or_else(|p| p.into_inner());
            self.commit_all(message)?
        };
        self.backend.fetch(&self.repo_dir)?;
        {
            let _g = io.lock().unwrap_or_else(|p| p.into_inner());
            self.backend.rebase(&self.repo_dir)?;
            if !committed && self.unpushed_count()? == 0 {
                return Ok(SyncStatus::NothingToPush);
            }
        }
        let rebased = self.push_staged(io)?;
        let auto_merged = self.backend.take_auto_merged();
        Ok(SyncStatus::Pushed { rebased, auto_merged })
    }

    pub fn pull_staged(&self, io: &std::sync::Mutex<()>) -> Result<Vec<String>, SyncError> {
        if !self.backend.supports_staged_sync() {
            let _g = io.lock().unwrap_or_else(|p| p.into_inner());
            return self.pull().map(|_| self.backend.take_auto_merged());
        }
        self.backend.recover_interrupted(&self.repo_dir)?;
        self.backend.fetch(&self.repo_dir)?;
        {
            let _g = io.lock().unwrap_or_else(|p| p.into_inner());
            self.backend.rebase(&self.repo_dir)?;
        }
        // pull 周期产出的自动合并提交必须当场推走：静默 pull 本身不推送，
        // 调度门禁又只看工作区是否脏（合并已提交 = 不脏），不推将永远滞留。
        let auto_merged = self.backend.take_auto_merged();
        if !auto_merged.is_empty() {
            self.push_staged(io)?;
        }
        Ok(auto_merged)
    }

    fn push_staged(&self, io: &std::sync::Mutex<()>) -> Result<bool, SyncError> {
        match self.backend.push(&self.repo_dir) {
            Ok(()) => Ok(false),
            Err(SyncError::Git(detail)) if is_push_rejected(&detail) => {
                self.backend.fetch(&self.repo_dir)?;
                {
                    let _g = io.lock().unwrap_or_else(|p| p.into_inner());
                    self.backend.rebase(&self.repo_dir)?;
                }
                self.backend.push(&self.repo_dir)?;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }
}

/// 系统 git CLI 后端（桌面）。子进程统一 CREATE_NO_WINDOW。
#[derive(Default)]
struct CliGitBackend {
    /// 最近一次语义自动合并成功写回的文件清单（take_auto_merged 消费后清空）。
    auto_merged: std::sync::Mutex<Vec<String>>,
}

/// 冲突自动合并总开关：`STICKY_AUTO_MERGE=0` 关闭（回退人工冲突态）。
fn auto_merge_enabled() -> bool {
    std::env::var("STICKY_AUTO_MERGE").map(|v| v != "0").unwrap_or(true)
}

/// 冲突备份根目录：QA 覆盖 → `%APPDATA%\sticky-todo` → 两者皆无（如安卓）
/// 返回 None，跳过备份只记日志。
fn backup_root() -> Option<PathBuf> {
    if let Ok(qa) = std::env::var("STICKY_QA_ROOT") {
        return Some(PathBuf::from(qa));
    }
    std::env::var("APPDATA").ok().map(|d| PathBuf::from(d).join("sticky-todo"))
}

/// 一个 unmerged 路径的三个 stage：[base, ours, theirs]＝(oid, mode)。
struct UnmergedPath {
    path: String,
    stages: [Option<(String, String)>; 3],
}

impl CliGitBackend {
    fn git(&self, cwd: &Path, args: &[&str]) -> Result<(), SyncError> {
        run_git(Some(cwd), args).map(|_| ()).map_err(|e| annotate(e, &args.join(" ")))
    }

    fn git_out(&self, cwd: &Path, args: &[&str]) -> Result<String, SyncError> {
        run_git(Some(cwd), args).map_err(|e| annotate(e, &args.join(" ")))
    }
    /// Reject a reused clone pointing at a different journal, including push-only URLs.
    fn validate_origin(&self, repo_dir: &Path, expected: &str) -> Result<(), SyncError> {
        for args in [
            vec!["remote", "get-url", "--all", "origin"],
            vec!["remote", "get-url", "--push", "--all", "origin"],
        ] {
            let urls = self.git_out(repo_dir, &args)?;
            if urls.is_empty() || urls.lines().any(|url| repository_id(url) != repository_id(expected)) {
                return Err(SyncError::Git("origin 与配置的工作日志仓库不一致；请检查本机仓库路径和远端配置".into()));
            }
        }
        self.tracking_ref(repo_dir)?;
        Ok(())
    }

    fn tracking_ref(&self, repo_dir: &Path) -> Result<String, SyncError> {
        let branch = self.git_out(repo_dir, &["symbolic-ref", "--short", "HEAD"])?;
        let remote = self.git_out(repo_dir, &["config", "--get", &format!("branch.{branch}.remote")])?;
        if remote != "origin" {
            return Err(SyncError::Git("当前分支必须跟踪 origin 工作日志仓库".into()));
        }
        let merge_ref = self.git_out(repo_dir, &["config", "--get", &format!("branch.{branch}.merge")])?;
        if !merge_ref.starts_with("refs/heads/") {
            return Err(SyncError::Git("工作日志分支没有有效的远端跟踪分支".into()));
        }
        self.git(repo_dir, &["check-ref-format", &merge_ref])?;
        Ok(merge_ref)
    }

    fn ensure_commit_identity(&self, repo_dir: &Path) -> Result<(), SyncError> {
        for (key, fallback) in [("user.name", "Sticky Todo"), ("user.email", "sticky-todo@localhost")] {
            // Effective config includes the user's global identity. Fill only missing/empty values.
            if self.git_out(repo_dir, &["config", "--get", key]).unwrap_or_default().trim().is_empty() {
                self.git(repo_dir, &["config", "--local", key, fallback])?;
            }
        }
        Ok(())
    }

    /// rebase 失败善后：冲突类先 abort 回干净态，再尝试语义自动合并；
    /// 成功 → Ok（后续 push 带上合并提交）；不可自动合并 → Conflict。
    fn after_rebase_failure(&self, repo_dir: &Path, err: SyncError, phase: &str) -> Result<(), SyncError> {
        let _ = self.git(repo_dir, &["rebase", "--abort"]);
        let is_conflict = matches!(&err, SyncError::Git(d) if d.contains("CONFLICT"));
        if is_conflict && auto_merge_enabled() {
            return match self.attempt_conflict_merge(repo_dir) {
                Ok(files) => {
                    *self.auto_merged.lock().unwrap_or_else(|p| p.into_inner()) = files;
                    Ok(())
                }
                Err(merge_err) => Err(SyncError::Conflict(format!(
                    "{phase} 遇到冲突，未能自动合并（需人工处理）: {merge_err}"
                ))),
            };
        }
        match err {
            SyncError::Git(detail) if detail.contains("CONFLICT") => {
                Err(SyncError::Conflict(format!("{phase} 遇到冲突: {detail}")))
            }
            other => Err(other),
        }
    }

    /// 语义合并尝试：`merge --no-commit` 取 unmerged 三方 → merge_resolve
    /// 纯函数解析 → 备份四方 + 写回 + add → 单个合并提交（两父）。
    /// 成功返回自动合并涉及（或合并带入）的文件清单；任何不可自动合并的
    /// 情况都 `merge --abort` 回到干净态再返回 Err。
    fn attempt_conflict_merge(&self, repo_dir: &Path) -> Result<Vec<String>, SyncError> {
        let pre_merge_head = self.git_out(repo_dir, &["rev-parse", "HEAD"]).unwrap_or_default();
        let merge_started =
            self.git(repo_dir, &["merge", "--no-commit", "--no-ff", "--no-edit", "@{upstream}"]).is_ok();
        let entries = self.collect_unmerged(repo_dir)?;
        if entries.is_empty() {
            if repo_dir.join(".git").join("MERGE_HEAD").exists() {
                // rebase 逐提交重放冲突，但整树三方 merge 无冲突 → 收尾提交即可
                self.ensure_commit_identity(repo_dir)?;
                self.git(repo_dir, &["commit", "--no-edit"])?;
                return self.merged_file_list(repo_dir, &pre_merge_head);
            }
            if !merge_started {
                return Err(SyncError::Git("merge 未能开始（工作区不干净或上游不可达）".into()));
            }
            return Ok(vec![]); // Already up to date：无需合并
        }
        let sides = self.load_conflict_sides(repo_dir, entries)?;
        let Some(resolved) = crate::merge_resolve::resolve_conflict_set(&sides) else {
            self.git(repo_dir, &["merge", "--abort"]).ok();
            return Err(SyncError::Git(
                "存在不可自动合并的文件（非文本/内容双改分歧/删除对修改）".into(),
            ));
        };
        if let Err(e) = crate::merge_resolve::write_backups(backup_root().as_deref(), &sides, &resolved) {
            log::error!(target: "sticky", "[sync] 冲突备份失败（不阻断合并）: {e}");
        }
        for r in &resolved {
            let target = repo_dir.join(&r.path);
            if let Err(e) = std::fs::write(&target, &r.content) {
                self.git(repo_dir, &["merge", "--abort"]).ok();
                return Err(SyncError::Git(format!("写回 {} 失败: {e}", r.path)));
            }
            self.git(repo_dir, &["add", "--", &r.path])?;
        }
        self.ensure_commit_identity(repo_dir)?;
        let message = format!("{} 自动合并同步冲突（{} 文件）", commit_prefix(), resolved.len());
        self.git(repo_dir, &["commit", "-m", &message])?;
        Ok(resolved.iter().map(|r| r.path.clone()).collect())
    }

    /// `git ls-files -u -z` → 每路径按 stage 1/2/3 收集 (oid, mode)。
    /// 仅 mode 100644/100755 参与自动合并；symlink/gitlink 路径照常收集
    /// （load_conflict_sides 里转成人工触发形态）。
    fn collect_unmerged(&self, repo_dir: &Path) -> Result<Vec<UnmergedPath>, SyncError> {
        let raw = run_git_raw(Some(repo_dir), &["ls-files", "-u", "-z"])?;
        let mut out: Vec<UnmergedPath> = Vec::new();
        for rec in raw.split(|&b| b == 0) {
            if rec.is_empty() {
                continue;
            }
            let Some(tab) = rec.iter().position(|&b| b == b'\t') else { continue };
            let meta = String::from_utf8_lossy(&rec[..tab]);
            let mut parts = meta.split_whitespace();
            let (Some(mode), Some(oid), Some(stage)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let Ok(stage) = stage.parse::<usize>() else { continue };
            let path = String::from_utf8_lossy(&rec[tab + 1..]).into_owned();
            let entry = match out.iter_mut().find(|e| e.path == path) {
                Some(e) => e,
                None => {
                    out.push(UnmergedPath { path: path.clone(), stages: Default::default() });
                    out.last_mut().expect("just pushed")
                }
            };
            if (1..=3).contains(&stage) {
                entry.stages[stage - 1] = Some((oid.to_string(), mode.to_string()));
            }
        }
        Ok(out)
    }

    /// unmerged 条目 → 三方字节。不支持的 mode 或缺失 ours/theirs stage
    /// 映射为对应侧 None（merge_resolve 判人工）；blob 经 cat-file 原样取回。
    fn load_conflict_sides(&self, repo_dir: &Path, entries: Vec<UnmergedPath>) -> Result<Vec<ConflictSides>, SyncError> {
        let text_mode = |m: &str| m == "100644" || m == "100755";
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let any_unsupported = e
                .stages
                .iter()
                .flatten()
                .any(|(_, m)| !text_mode(m));
            let blob = |oid: &Option<(String, String)>| -> Option<Vec<u8>> {
                let (oid, mode) = oid.as_ref()?;
                if !text_mode(mode) {
                    return None; // symlink 等 → 人工
                }
                run_git_raw(Some(repo_dir), &["cat-file", "blob", oid]).ok()
            };
            let (mut base, mut ours, mut theirs) =
                (blob(&e.stages[0]), blob(&e.stages[1]), blob(&e.stages[2]));
            if any_unsupported {
                // 该文件存在非文本 stage：整体降级人工（ours=None 触发）
                base = None;
                ours = None;
                theirs = None;
            }
            out.push(ConflictSides { path: e.path, base, ours, theirs });
        }
        Ok(out)
    }

    /// 干净 merge 收尾后的文件清单（pre..HEAD diff）。
    fn merged_file_list(&self, repo_dir: &Path, pre_merge_head: &str) -> Result<Vec<String>, SyncError> {
        if pre_merge_head.is_empty() {
            return Ok(vec![]);
        }
        let raw = run_git_raw(Some(repo_dir), &["diff", "--name-only", "-z", pre_merge_head, "HEAD"])?;
        Ok(raw
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }
}

impl GitBackend for CliGitBackend {
    fn supports_staged_sync(&self) -> bool { true }

    fn fetch(&self, repo_dir: &Path) -> Result<(), SyncError> {
        self.git(repo_dir, &["fetch", "origin"])
    }

    fn rebase(&self, repo_dir: &Path) -> Result<(), SyncError> {
        self.tracking_ref(repo_dir)?;
        match self.git(repo_dir, &["rebase", "@{upstream}"]) {
            Ok(_) => Ok(()),
            Err(e) => self.after_rebase_failure(repo_dir, e, "pull"),
        }
    }

    fn recover_interrupted(&self, repo_dir: &Path) -> Result<(), SyncError> {
        let dot_git = repo_dir.join(".git");
        if !dot_git.exists() {
            return Ok(());
        }
        // 前次同步在 merge/commit 之间中断 → merge --abort 回干净态再走正常流。
        // abort 失败或 MERGE_HEAD 仍在（如工作区被中断后的编辑弄脏）→ 拒绝本周期。
        if dot_git.join("MERGE_HEAD").exists() {
            if let Err(e) = self.git(repo_dir, &["merge", "--abort"]) {
                return Err(SyncError::Conflict(format!(
                    "上次同步中断的合并状态无法自动清理（merge --abort 失败），需人工处理: {e}"
                )));
            }
            if dot_git.join("MERGE_HEAD").exists() {
                return Err(SyncError::Conflict(
                    "上次同步中断的合并状态未能清理（MERGE_HEAD 仍存在），需人工处理".into(),
                ));
            }
        }
        // rebase 半程态（rebase-merge / rebase-apply），同样清不掉即拒绝
        if dot_git.join("rebase-merge").exists() || dot_git.join("rebase-apply").exists() {
            if let Err(e) = self.git(repo_dir, &["rebase", "--abort"]) {
                return Err(SyncError::Conflict(format!(
                    "上次同步中断的 rebase 状态无法自动清理（rebase --abort 失败），需人工处理: {e}"
                )));
            }
            if dot_git.join("rebase-merge").exists() || dot_git.join("rebase-apply").exists() {
                return Err(SyncError::Conflict(
                    "上次同步中断的 rebase 状态未能清理，需人工处理".into(),
                ));
            }
        }
        Ok(())
    }

    fn take_auto_merged(&self) -> Vec<String> {
        std::mem::take(&mut *self.auto_merged.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// 目录已是 git 仓库 → Ok(false)；不存在 → 从 url 克隆 → Ok(true)。
    fn ensure_cloned(&self, repo_dir: &Path, url: &str) -> Result<bool, SyncError> {
        if repo_dir.join(".git").exists() {
            self.validate_origin(repo_dir, url)?;
            return Ok(false);
        }
        if repo_dir.exists() {
            return Err(SyncError::Git(format!(
                "{} 存在但不是 git 仓库",
                repo_dir.display()
            )));
        }
        run_git(None, &["clone", "--", url, &repo_dir.to_string_lossy()])
            .map_err(|e| annotate(e, "clone"))?;
        self.validate_origin(repo_dir, url)?;
        Ok(true)
    }

    fn pull_rebase(&self, repo_dir: &Path) -> Result<(), SyncError> {
        let merge_ref = self.tracking_ref(repo_dir)?;
        match self.git(repo_dir, &["pull", "--rebase", "origin", &merge_ref]) {
            Ok(_) => Ok(()),
            Err(e) => self.after_rebase_failure(repo_dir, e, "pull"),
        }
    }

    fn commit_all(&self, repo_dir: &Path, message: &str) -> Result<bool, SyncError> {
        // 先用 porcelain（locale 无关）判干净：不能依赖 "nothing to commit" 文案，
        // gettext 本地化的 git（如 zh-CN）文案不同 → 误判失败 → 假离线 30s 死循环。
        if !self.has_local_changes(repo_dir) {
            return Ok(false);
        }
        self.ensure_commit_identity(repo_dir)?;
        self.git(repo_dir, &["add", "-A"])?;
        match self.git(repo_dir, &["commit", "-m", message]) {
            Ok(_) => Ok(true),
            Err(SyncError::Git(detail)) if detail.contains("nothing to commit") => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn push(&self, repo_dir: &Path) -> Result<(), SyncError> {
        let merge_ref = self.tracking_ref(repo_dir)?;
        self.git(repo_dir, &["push", "origin", &format!("HEAD:{merge_ref}")])
    }

    fn push_with_retry(&self, repo_dir: &Path) -> Result<bool, SyncError> {
        match self.push(repo_dir) {
            Ok(()) => Ok(false),
            Err(SyncError::Git(detail)) if is_push_rejected(&detail) => {
                self.pull_rebase(repo_dir)?;
                self.push(repo_dir)?;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    fn unpushed_count(&self, repo_dir: &Path) -> Result<usize, SyncError> {
        let out = self.git_out(repo_dir, &["rev-list", "--count", "@{upstream}..HEAD"])?;
        out.trim().parse().map_err(|_| {
            SyncError::Git(format!("rev-list 输出无法解析: {out:?}"))
        })
    }

    fn has_local_changes(&self, repo_dir: &Path) -> bool {
        self.git_out(repo_dir, &["status", "--porcelain"]).map(|o| !o.is_empty()).unwrap_or(true)
    }

}

fn repository_id(url: &str) -> String {
    // Git stores Windows paths with '/', even when the configured path uses '\\'.
    // Compare existing local remotes by their resolved filesystem identity.
    if let Ok(path) = std::fs::canonicalize(url.trim()) {
        let id = path.to_string_lossy().into_owned();
        return if cfg!(windows) { id.to_lowercase() } else { id };
    }
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    // HTTPS and SSH URLs for the same GitHub owner/repo refer to the same journal.
    for prefix in ["https://github.com/", "ssh://git@github.com/", "git@github.com:"] {
        if let Some(path) = url.strip_prefix(prefix) {
            return format!("github.com/{}", path.to_ascii_lowercase());
        }
    }
    url.to_string()
}

/// 与 [run_git] 相同，但返回原始字节（不 trim、不归一化行尾/编码）。
/// 仅用于 `cat-file` / `ls-files -z` 等需要字节保真的读操作。
fn run_git_raw(cwd: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, SyncError> {
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => return Err(SyncError::GitUnavailable(e.to_string())),
    };
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut detail = format!("exit={:?} args={args:?}", output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stdout.is_empty() {
        detail.push_str(" stdout=");
        detail.push_str(&stdout);
    }
    if !stderr.is_empty() {
        detail.push_str(" stderr=");
        detail.push_str(&stderr);
    }
    Err(SyncError::Git(detail))
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String, SyncError> {
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => return Err(SyncError::GitUnavailable(e.to_string())),
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        return Ok(stdout);
    }
    let mut detail = format!("exit={:?} args={args:?}", output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stdout.is_empty() {
        detail.push_str(" stdout=");
        detail.push_str(&stdout);
    }
    if !stderr.is_empty() {
        detail.push_str(" stderr=");
        detail.push_str(&stderr);
    }
    Err(SyncError::Git(detail))
}

fn annotate(err: SyncError, phase: &str) -> SyncError {
    match err {
        SyncError::Git(d) => SyncError::Git(format!("[{phase}] {d}")),
        other => other,
    }
}

/// push 被拒 = 需要先 fetch。识别 git 的常见拒绝措辞。
fn is_push_rejected(detail: &str) -> bool {
    let d = detail.to_lowercase();
    d.contains("non-fast-forward") || d.contains("fetch first") || d.contains("rejected")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个 bare "远端" + 两个克隆，模拟多端并发。
    fn setup_two_party(name: &str) -> (PathBuf, PathBuf, String) {
        let base = std::env::temp_dir().join(format!("sticky-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let remote = base.join("remote.git");
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&base).unwrap();
        let url = format!("file:///{}", remote.to_string_lossy().replace('\\', "/"));

        run_git(Some(&base), &["init", "--bare", &remote.to_string_lossy()]).unwrap();
        // a 首克隆 + 初始提交，b 在远端有内容后再克隆（避开空仓库 unborn 分支）
        // The gix backend preserves raw bytes; isolate fixtures from host CRLF defaults.
        run_git(None, &["clone", "--config", "core.autocrlf=false", &url, &a.to_string_lossy()]).unwrap();
        configure_identity(&a);
        std::fs::write(a.join("README.md"), "# test\n").unwrap();
        run_git(Some(&a), &["add", "-A"]).unwrap();
        run_git(Some(&a), &["commit", "-m", "init"]).unwrap();
        run_git(Some(&a), &["push"]).unwrap();
        run_git(None, &["clone", "--config", "core.autocrlf=false", &url, &b.to_string_lossy()]).unwrap();
        configure_identity(&b);
        (a, b, url)
    }

    fn configure_identity(dir: &Path) {
        run_git(Some(dir), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(dir), &["config", "user.email", "test@example.com"]).unwrap();
    }

    fn write_file(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn cli_backend_supports_staged() {
        assert!(CliGitBackend::default().supports_staged_sync());
    }

    #[test]
    fn staged_roundtrip_push_pull() {
        let (a, b, _) = setup_two_party("staged-roundtrip");
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let eng_b = SyncEngine::with_backend(&b, Box::new(CliGitBackend::default()));
        let io = std::sync::Mutex::new(());
        write_file(&a, "a.md", "from a\n");
        eng_a.commit_all("a write").unwrap();
        eng_a.push().unwrap();
        write_file(&b, "b.md", "from b\n");
        let st = eng_b.sync_cycle_staged("b write", &io).unwrap();
        assert_eq!(st, SyncStatus::Pushed { rebased: false, auto_merged: vec![] });
        assert_eq!(std::fs::read_to_string(b.join("a.md")).unwrap(), "from a\n");
        eng_a.pull_staged(&io).unwrap();
        assert_eq!(std::fs::read_to_string(a.join("b.md")).unwrap(), "from b\n");
        assert_eq!(eng_b.sync_cycle_staged("empty", &io).unwrap(), SyncStatus::NothingToPush);
    }

    #[test]
    fn staged_push_rejected_rebases() {
        // 远端在 fetch 后推进，让第一次 push 确实被拒；同时检查每段锁范围。
        struct AdvanceRemoteAfterFetch {
            a: PathBuf,
            io: std::sync::Arc<std::sync::Mutex<()>>,
        }

        impl GitBackend for AdvanceRemoteAfterFetch {
            fn supports_staged_sync(&self) -> bool { true }
            fn ensure_cloned(&self, repo_dir: &Path, url: &str) -> Result<bool, SyncError> {
                CliGitBackend::default().ensure_cloned(repo_dir, url)
            }
            fn fetch(&self, repo_dir: &Path) -> Result<(), SyncError> {
                assert!(self.io.try_lock().is_ok());
                CliGitBackend::default().fetch(repo_dir)?;
                CliGitBackend::default().push(&self.a)
            }
            fn rebase(&self, repo_dir: &Path) -> Result<(), SyncError> {
                assert!(matches!(self.io.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
                CliGitBackend::default().rebase(repo_dir)
            }
            fn pull_rebase(&self, repo_dir: &Path) -> Result<(), SyncError> {
                CliGitBackend::default().pull_rebase(repo_dir)
            }
            fn commit_all(&self, repo_dir: &Path, message: &str) -> Result<bool, SyncError> {
                assert!(matches!(self.io.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
                CliGitBackend::default().commit_all(repo_dir, message)
            }
            fn push(&self, repo_dir: &Path) -> Result<(), SyncError> {
                assert!(self.io.try_lock().is_ok());
                CliGitBackend::default().push(repo_dir)
            }
            fn push_with_retry(&self, repo_dir: &Path) -> Result<bool, SyncError> {
                CliGitBackend::default().push_with_retry(repo_dir)
            }
            fn unpushed_count(&self, repo_dir: &Path) -> Result<usize, SyncError> {
                CliGitBackend::default().unpushed_count(repo_dir)
            }
            fn has_local_changes(&self, repo_dir: &Path) -> bool {
                CliGitBackend::default().has_local_changes(repo_dir)
            }
        }

        let (a, b, _) = setup_two_party("staged-rejected");
        let io = std::sync::Arc::new(std::sync::Mutex::new(()));
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let eng_b = SyncEngine::with_backend(&b, Box::new(AdvanceRemoteAfterFetch {
            a: a.clone(), io: io.clone(),
        }));
        write_file(&a, "a.md", "from a\n");
        eng_a.commit_all("a write").unwrap();
        write_file(&b, "b.md", "from b\n");
        let st = eng_b.sync_cycle_staged("b write", &io).unwrap();
        assert_eq!(st, SyncStatus::Pushed { rebased: true, auto_merged: vec![] });
        eng_a.pull_staged(&io).unwrap();
        assert_eq!(std::fs::read_to_string(a.join("b.md")).unwrap(), "from b\n");
        assert_eq!(std::fs::read_to_string(b.join("a.md")).unwrap(), "from a\n");
    }

    #[test]
    fn staged_conflict_aborts_clean() {
        // scripts/tool.py 非文本白名单 → 不可自动合并 → 人工冲突态
        let (a, b, _) = setup_two_party("staged-conflict");
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let eng_b = SyncEngine::with_backend(&b, Box::new(CliGitBackend::default()));
        let io = std::sync::Mutex::new(());
        write_file(&a, "scripts/tool.py", "print('from a')\n");
        eng_a.commit_all("a conflict").unwrap();
        eng_a.push().unwrap();
        write_file(&b, "scripts/tool.py", "print('from b')\n");
        let err = eng_b.sync_cycle_staged("b conflict", &io).unwrap_err();
        assert!(matches!(err, SyncError::Conflict(_)));
        assert!(run_git(Some(&b), &["status", "--porcelain"]).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(b.join("scripts/tool.py")).unwrap(), "print('from b')\n");
        // 中断态已清理：无 MERGE_HEAD、无 rebase 残留
        assert!(!b.join(".git").join("MERGE_HEAD").exists());
    }

    /// 真实冲突链路：双端改同一任务行不同属性 → rebase 冲突 → 语义自动合并
    /// → 两父合并提交 → push 成功 → 对端可见。
    #[test]
    fn staged_conflict_auto_resolves_and_pushes() {
        let (a, b, _) = setup_two_party("auto-merge");
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let eng_b = SyncEngine::with_backend(&b, Box::new(CliGitBackend::default()));
        let io = std::sync::Mutex::new(());
        let day = "days/2026-09-11.md";
        let base = "# 2026-09-11\n\n## 日任务\n- [ ] (P1) 任务A\n";
        write_file(&a, day, base);
        eng_a.commit_all("seed").unwrap();
        eng_a.push().unwrap();
        eng_b.pull_staged(&io).unwrap();

        // a 勾选；b 调优先级——对 git 是同行双改冲突，对语义是两个可仲裁属性
        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [x] (P1) 任务A\n");
        eng_a.commit_all("a check").unwrap();
        eng_a.push().unwrap();
        write_file(&b, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P0) 任务A\n");
        let st = eng_b.sync_cycle_staged("b prio", &io).unwrap();
        assert_eq!(
            st,
            SyncStatus::Pushed { rebased: false, auto_merged: vec![day.to_string()] }
        );
        // 工作区干净，合并结果=勾选+新优先级
        assert!(run_git(Some(&b), &["status", "--porcelain"]).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(b.join(day)).unwrap(),
            "# 2026-09-11\n\n## 日任务\n- [x] (P0) 任务A\n"
        );
        // 合并提交有两个父提交
        let parents = run_git(Some(&b), &["log", "-1", "--format=%P"]).unwrap();
        assert_eq!(parents.split_whitespace().count(), 2, "自动合并提交应为两父: {parents}");
        // 推送真实落地：a 拉取可见合并结果
        eng_a.pull_staged(&io).unwrap();
        assert_eq!(
            std::fs::read_to_string(a.join(day)).unwrap(),
            "# 2026-09-11\n\n## 日任务\n- [x] (P0) 任务A\n"
        );
    }

    /// 冲突在 pull 路径（静默拉取）同样自动合并。
    #[test]
    fn pull_staged_auto_resolves() {
        let (a, b, _) = setup_two_party("pull-automerge");
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let eng_b = SyncEngine::with_backend(&b, Box::new(CliGitBackend::default()));
        let io = std::sync::Mutex::new(());
        let day = "days/2026-09-11.md";
        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P1) 任务A\n");
        eng_a.commit_all("seed").unwrap();
        eng_a.push().unwrap();
        eng_b.pull_staged(&io).unwrap();

        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [x] (P1) 任务A\n");
        eng_a.commit_all("a check").unwrap();
        eng_a.push().unwrap();
        write_file(&b, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P0) 任务A\n");
        eng_b.commit_all("b prio").unwrap();
        let merged = eng_b.pull_staged(&io).unwrap();
        assert_eq!(merged, vec![day.to_string()]);
        assert!(run_git(Some(&b), &["status", "--porcelain"]).unwrap().is_empty());
        assert!(std::fs::read_to_string(b.join(day)).unwrap().contains("- [x] (P0) 任务A"));
        // 合并提交已当场推走（静默 pull 不推送，滞留将无人再推）
        assert_eq!(eng_b.unpushed_count().unwrap(), 0);
        eng_a.pull_staged(&io).unwrap();
        assert_eq!(
            std::fs::read_to_string(a.join(day)).unwrap(),
            std::fs::read_to_string(b.join(day)).unwrap()
        );
    }

    /// 崩溃残留清不掉（merge --abort 失败、MERGE_HEAD 仍在）→ 拒绝本周期
    /// 进人工冲突态，绝不 add -A 提交带冲突标记的工作区。
    /// 复现真实失败形态：真冲突半程态 + 暂存新文件后又改（AM，"not
    /// uptodate"）→ git merge --abort 必败。
    #[test]
    fn uncleanable_merge_state_blocks_sync() {
        let (a, b, _) = setup_two_party("merge-abort-fail");
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let io = std::sync::Mutex::new(());
        let day = "days/2026-09-11.md";
        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P1) 任务A\n");
        run_git(Some(&a), &["add", "-A"]).unwrap();
        run_git(Some(&a), &["commit", "-m", "seed"]).unwrap();
        run_git(Some(&a), &["push", "-q"]).unwrap();
        // b 先拉 seed 推勾选；a 本地改优先级并手工 merge --no-commit 制造半程态
        run_git(Some(&b), &["pull", "-q"]).unwrap();
        write_file(&b, day, "# 2026-09-11\n\n## 日任务\n- [x] (P1) 任务A\n");
        run_git(Some(&b), &["add", "-A"]).unwrap();
        run_git(Some(&b), &["commit", "-m", "b check"]).unwrap();
        run_git(Some(&b), &["push", "-q"]).unwrap();
        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P0) 任务A\n");
        run_git(Some(&a), &["add", "-A"]).unwrap();
        run_git(Some(&a), &["commit", "-m", "a prio"]).unwrap();
        run_git(Some(&a), &["fetch", "-q"]).unwrap();
        let _ = run_git(Some(&a), &["merge", "--no-commit", "FETCH_HEAD"]);
        assert!(a.join(".git").join("MERGE_HEAD").exists(), "应有半程态");
        // AM：暂存新文件后再改 → merge --abort 因 not uptodate 必败
        std::fs::write(a.join("staged.txt"), "x\n").unwrap();
        run_git(Some(&a), &["add", "staged.txt"]).unwrap();
        std::fs::write(a.join("staged.txt"), "x\ny\n").unwrap();

        match eng_a.sync_cycle_staged("t", &io) {
            Err(SyncError::Conflict(m)) => assert!(m.contains("人工处理"), "{m}"),
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
        // 未走 add -A/commit/push：半程态仍在、未推提交仍只有 a prio 一条
        assert!(a.join(".git").join("MERGE_HEAD").exists(), "半程态应保留");
        assert_eq!(eng_a.unpushed_count().unwrap(), 1);
    }

    /// 自动合并提交在克隆从未配置本地身份时回落 sticky-todo@localhost
    /// （新机器首装场景；空 local 身份 + useConfigOnly 掩蔽本机全局配置）。
    #[test]
    fn automerge_commit_identity_fallback() {
        let (a, b, _) = setup_two_party("automerge-identity");
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let eng_b = SyncEngine::with_backend(&b, Box::new(CliGitBackend::default()));
        let io = std::sync::Mutex::new(());
        for dir in [&a, &b] {
            run_git(Some(dir), &["config", "user.name", ""]).unwrap();
            run_git(Some(dir), &["config", "user.email", ""]).unwrap();
            run_git(Some(dir), &["config", "user.useConfigOnly", "true"]).unwrap();
        }
        let day = "days/2026-09-11.md";
        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P1) 任务A\n");
        eng_a.commit_all("seed").unwrap();
        eng_a.push().unwrap();
        eng_b.pull_staged(&io).unwrap();
        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [x] (P1) 任务A\n");
        eng_a.commit_all("a check").unwrap();
        eng_a.push().unwrap();
        write_file(&b, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P0) 任务A\n");
        eng_b.sync_cycle_staged("b prio", &io).unwrap();
        let email = run_git(Some(&b), &["log", "-1", "--format=%ae"]).unwrap();
        assert_eq!(email, "sticky-todo@localhost");
    }

    /// 崩溃恢复：前次中断留下的 merge 半程态在同步入口被清掉，周期正常完成。
    #[test]
    fn interrupted_merge_state_recovered_at_entry() {
        let (a, b, _) = setup_two_party("crash-recover");
        let eng_a = SyncEngine::with_backend(&a, Box::new(CliGitBackend::default()));
        let eng_b = SyncEngine::with_backend(&b, Box::new(CliGitBackend::default()));
        let io = std::sync::Mutex::new(());
        let day = "days/2026-09-11.md";
        write_file(&a, day, "# 2026-09-11\n\n## 日任务\n- [ ] (P1) 任务A\n");
        eng_a.commit_all("seed").unwrap();
        eng_a.push().unwrap();
        eng_b.pull_staged(&io).unwrap();

        // 人为制造中断残留：对端推进 + 本地 merge --no-commit 停在半程
        write_file(&a, "days/2026-09-12.md", "# 2026-09-12\n");
        eng_a.commit_all("a next day").unwrap();
        eng_a.push().unwrap();
        eng_b.backend.fetch(&b).unwrap();
        run_git(Some(&b), &["merge", "--no-commit", "--no-ff", "--no-edit", "@{upstream}"]).ok();
        assert!(b.join(".git").join("MERGE_HEAD").exists());

        // 正常同步入口应先 abort 半程态再完成周期
        write_file(&b, day, "# 2026-09-11\n\n## 日任务\n- [x] (P1) 任务A\n");
        let st = eng_b.sync_cycle_staged("b edit after crash", &io).unwrap();
        assert!(matches!(st, SyncStatus::Pushed { .. }));
        assert!(!b.join(".git").join("MERGE_HEAD").exists());
        assert!(b.join("days/2026-09-12.md").exists());
        assert!(std::fs::read_to_string(b.join(day)).unwrap().contains("- [x] (P1) 任务A"));
    }

    #[test]
    fn commit_and_push_roundtrip() {
        let (a, b, _url) = setup_two_party("roundtrip");
        let eng = SyncEngine::new(&a);

        write_file(&a, "days/2026-09-10.md", "# x\n");
        let st = eng.sync_cycle(&format!("{} test write", commit_prefix())).unwrap();
        assert_eq!(st, SyncStatus::Pushed { rebased: false, auto_merged: vec![] });

        // 另一端拉取可见
        run_git(Some(&b), &["pull"]).unwrap();
        assert!(b.join("days/2026-09-10.md").exists());
    }

    #[test]
    fn nothing_to_push_is_ok() {
        let (a, _b, _url) = setup_two_party("noop");
        let eng = SyncEngine::new(&a);
        let st = eng.sync_cycle(&format!("{} empty", commit_prefix())).unwrap();
        assert_eq!(st, SyncStatus::NothingToPush);
    }

    /// 生产路径回归：编辑一个已被跟踪的文件（工作区有 unstaged 改动）后
    /// 直接走完整周期。旧实现先 pull --rebase，会因 "You have unstaged
    /// changes" exit 128 永远重试失败（2026-09-10 实测）。
    #[test]
    fn modified_tracked_file_syncs() {
        let (a, b, _url) = setup_two_party("dirty-tracked");
        let eng = SyncEngine::new(&a);

        // 第一轮：建文件并提交推送 → days/2026-09-10.md 进入 tracked
        write_file(&a, "days/2026-09-10.md", "# v1\n\n## 日任务\n- [ ] (P1) 旧\n");
        eng.sync_cycle(&format!("{} first", commit_prefix())).unwrap();

        // 生产等价场景：store::edit 直接改写 tracked 文件（未 commit）
        write_file(&a, "days/2026-09-10.md", "# v1\n\n## 日任务\n- [x] (P1) 新\n");
        let st = eng.sync_cycle(&format!("{} second", commit_prefix())).unwrap();
        assert_eq!(st, SyncStatus::Pushed { rebased: false, auto_merged: vec![] });

        // 另一端可见新内容
        run_git(Some(&b), &["pull"]).unwrap();
        let got = std::fs::read_to_string(b.join("days/2026-09-10.md")).unwrap();
        assert!(got.contains("[x] (P1) 新"), "远端应看到勾选后的内容: {got}");
    }

    /// 离线积压：先本地提交多次，再走一次完整周期全部推上去。
    #[test]
    fn offline_backlog_gets_pushed() {
        let (a, _b, _url) = setup_two_party("backlog");
        let eng = SyncEngine::new(&a);
        write_file(&a, "days/2026-09-10.md", "# 1\n");
        eng.commit_all(&format!("{} offline 1", commit_prefix())).unwrap();
        write_file(&a, "days/2026-09-11.md", "# 2\n");
        eng.commit_all(&format!("{} offline 2", commit_prefix())).unwrap();

        // 本轮无新改动，但积压的 2 个提交必须被推送
        let st = eng.sync_cycle(&format!("{} back online", commit_prefix())).unwrap();
        assert_eq!(st, SyncStatus::Pushed { rebased: false, auto_merged: vec![] });
    }

    /// 外部并发场景：a 干净拉取后、push 前，b 抢先推送 → push 被拒 → rebase 重试成功。
    #[test]
    fn push_rejection_rebases_and_retries() {
        let (a, b, _url) = setup_two_party("external");
        let eng_a = SyncEngine::new(&a);

        // a 本地改文件并提交（模拟离线编辑后恢复在线）
        write_file(&a, "days/2026-09-10.md", "# a\n\n## 日任务\n- [ ] (P1) 任务A\n");
        eng_a.commit_all(&format!("{} a writes", commit_prefix())).unwrap();
        // a 拉取（此时远端只有 init，干净 rebase）
        eng_a.pull().unwrap();

        // b 抢先推送（模拟外部例行提交，发生在 a 的 pull 之后、push 之前）
        write_file(&b, "weeks/2026-W37.md", "# week\n");
        run_git(Some(&b), &["add", "-A"]).unwrap();
        run_git(Some(&b), &["commit", "-m", "external: nightly"]).unwrap();
        run_git(Some(&b), &["push"]).unwrap();

        // 直接 push 会被拒
        assert!(eng_a.push().is_err(), "此时 push 应被拒");
        // push_with_retry：rebase 掉 b 的提交后重试成功
        let rebased = eng_a.push_with_retry().unwrap();
        assert!(rebased, "应报告经历了 rebase");
        // b 能拉到 a 的文件
        run_git(Some(&b), &["pull"]).unwrap();
        assert!(b.join("days/2026-09-10.md").exists());
    }

    /// 真冲突（两端改同一行）→ Conflict 错误，仓库保持干净可重试。
    #[test]
    fn real_conflict_surfaces_as_conflict() {
        let (a, b, _url) = setup_two_party("conflict");
        let eng_a = SyncEngine::new(&a);

        // 同一文件的同一行，两端各改一版
        write_file(&a, "days/2026-09-11.md", "# 冲突\n\n## 日任务\n- [ ] (P1) A 的版本\n");
        eng_a.commit_all(&format!("{} a version", commit_prefix())).unwrap();

        write_file(&b, "days/2026-09-11.md", "# 冲突\n\n## 日任务\n- [ ] (P1) B 的版本\n");
        run_git(Some(&b), &["add", "-A"]).unwrap();
        run_git(Some(&b), &["commit", "-m", "b version"]).unwrap();
        run_git(Some(&b), &["push"]).unwrap();

        // a 走完整周期 → pull 阶段就撞冲突
        let err = eng_a.sync_cycle(&format!("{} a retry", commit_prefix())).unwrap_err();
        assert!(matches!(err, SyncError::Conflict(_)), "实际: {err:?}");
        // 仓库不在 rebase 中间态（后续操作仍可用）
        run_git(Some(&a), &["status"]).unwrap();
    }

    #[test]
    fn ensure_cloned_idempotent() {
        let (a, _b, url) = setup_two_party("clone");
        // Keep this clone inside the fixture rebuilt by setup_two_party.
        // Windows reuses process IDs, so a separate PID-only path can survive a prior run.
        let eng = SyncEngine::new(a.parent().unwrap().join("fresh"));
        assert!(eng.ensure_cloned(&url).unwrap(), "首次克隆");
        assert!(!eng.ensure_cloned(&url).unwrap(), "二次调用直接复用");
        assert!(eng.repo_dir().join(".git").exists());
    }

    #[test]
    fn machine_tag_resolution_order() {
        use super::resolve_machine_tag;
        // env 优先（含空白裁剪、空串跳过）
        assert_eq!(resolve_machine_tag(Some(" laptop "), Some("filetag"), "HOST-PC"), "laptop");
        assert_eq!(resolve_machine_tag(Some("  "), Some("filetag"), "HOST-PC"), "filetag");
        // 文件次之；最后回退主机名（小写 + 截 12 字符）
        assert_eq!(resolve_machine_tag(None, Some("tablet"), "HOST-PC"), "tablet");
        assert_eq!(resolve_machine_tag(None, Some("\u{feff}laptop\r\n"), "HOST-PC"), "laptop");
        assert_eq!(resolve_machine_tag(None, None, "WIN-DEMO-PC"), "win-demo-pc");
        assert_eq!(resolve_machine_tag(None, None, ""), "");
    }

    #[test]
    fn commit_without_configured_identity_uses_local_fallback() {
        let (a, _b, _) = setup_two_party("fresh-identity");
        // Empty local values mask this test machine's global identity.
        run_git(Some(&a), &["config", "user.name", ""]).unwrap();
        run_git(Some(&a), &["config", "user.email", ""]).unwrap();
        run_git(Some(&a), &["config", "user.useConfigOnly", "true"]).unwrap();
        write_file(&a, "days/2026-09-11.md", "# fresh computer\n");
        let eng = SyncEngine::new(&a);
        assert!(eng.commit_all("sticky@test: first edit").unwrap());
        assert_eq!(run_git(Some(&a), &["config", "--local", "user.name"]).unwrap(), "Sticky Todo");
        assert_eq!(run_git(Some(&a), &["config", "--local", "user.email"]).unwrap(), "sticky-todo@localhost");
    }

    #[test]
    fn ensure_cloned_rejects_unexpected_fetch_and_push_destinations() {
        let (a, _b, url) = setup_two_party("wrong-origin");
        let eng = SyncEngine::new(&a);
        assert!(eng.ensure_cloned("file:///unexpected-repository.git").is_err());
        run_git(Some(&a), &["remote", "set-url", "--push", "origin", "file:///unexpected-repository.git"]).unwrap();
        assert!(eng.ensure_cloned(&url).is_err());
        assert_eq!(run_git(Some(&a), &["remote", "get-url", "origin"]).unwrap(), url);
    }

    #[test]
    fn two_computers_converge_day_and_week_after_offline_edits() {
        let (a, b, url) = setup_two_party("two-computers");
        let workstation = SyncEngine::new(&a);
        let laptop = SyncEngine::new(&b);
        let day = "days/2026-09-11.md";
        let week = "weeks/2026-W37.md";
        // Windows Git may check out CRLF while locally edited files still use LF.
        let read_text = |root: &Path, path: &str| std::fs::read_to_string(root.join(path)).unwrap().replace("\r\n", "\n");
        write_file(&a, day, "# Friday\n\n## 日任务\n- [ ] (P1) workstation task\n");
        write_file(&a, week, "# Week 37\n\n## 本周任务\n- [ ] (P1) weekly task\n");
        workstation.sync_cycle("sticky@workstation: seed day and week").unwrap();
        laptop.sync_cycle("sticky@laptop: pull").unwrap();
        assert_eq!(read_text(&a, day), read_text(&b, day));
        assert_eq!(read_text(&a, week), read_text(&b, week));

        // An unavailable local URL simulates network loss; only throwaway bare repositories are used.
        run_git(Some(&b), &["remote", "set-url", "origin", "file:///sticky-nonexistent-offline-test.git"]).unwrap();
        write_file(&b, day, "# Friday\n\n## 日任务\n- [x] (P1) workstation task\n");
        write_file(&b, week, "# Week 37\n\n## 本周任务\n- [x] (P1) weekly task\n");
        assert!(laptop.sync_cycle("sticky@laptop: offline day and week").is_err());
        assert!(!laptop.has_local_changes(), "offline edits must already be committed");
        assert_eq!(laptop.unpushed_count().unwrap(), 1);

        write_file(&a, "days/2026-09-12.md", "# Saturday\n\n## 日任务\n- [ ] (P2) concurrent task\n");
        workstation.sync_cycle("sticky@workstation: concurrent next day").unwrap();
        run_git(Some(&b), &["remote", "set-url", "origin", &url]).unwrap();
        laptop.sync_cycle("sticky@laptop: reconnect").unwrap();
        workstation.sync_cycle("sticky@workstation: receive laptop edits").unwrap();
        for path in [day, week, "days/2026-09-12.md"] {
            assert_eq!(read_text(&a, path), read_text(&b, path));
        }
        assert_eq!(run_git(Some(&a), &["rev-parse", "HEAD"]).unwrap(), run_git(Some(&b), &["rev-parse", "HEAD"]).unwrap());
        assert_eq!(workstation.unpushed_count().unwrap(), 0);
        assert_eq!(laptop.unpushed_count().unwrap(), 0);
        let history = run_git(Some(&a), &["log", "--format=%s"]).unwrap();
        assert!(history.contains("sticky@workstation:") && history.contains("sticky@laptop:"));
        assert_eq!(run_git(Some(&a), &["config", "user.name"]).unwrap(), "Test");
        assert_eq!(run_git(Some(&a), &["config", "user.email"]).unwrap(), "test@example.com");
    }

    #[test]
    fn github_transport_spellings_identify_the_same_journal() {
        assert_eq!(repository_id("https://github.com/your-account/sticky-sync.git"), repository_id("git@github.com:your-account/sticky-sync.git"));
        assert_eq!(repository_id("https://github.com/your-account/sticky-sync/"), repository_id("ssh://git@github.com/your-account/sticky-sync.git"));
        assert_ne!(repository_id("https://github.com/your-account/other.git"), repository_id("https://github.com/your-account/sticky-sync.git"));
    }

    #[test]
    fn local_remote_accepts_windows_path_separator_variants() {
        let (a, _, _) = setup_two_party("path-spelling");
        let remote = a.parent().unwrap().join("remote.git");
        let eng = SyncEngine::new(&a);
        let git_path = remote.to_string_lossy().replace('\\', "/");
        run_git(Some(&a), &["remote", "set-url", "origin", &git_path]).unwrap();
        // Git's slash spelling and the native path name the same bare repository.
        assert_eq!(repository_id(&git_path), repository_id(&remote.to_string_lossy()));
        eng.ensure_cloned(&remote.to_string_lossy()).unwrap();
    }

    #[test]
    fn sync_binds_origin_even_when_another_push_remote_is_configured() {
        let (a, b, url) = setup_two_party("push-remote-override");
        let eng = SyncEngine::new(&a);
        run_git(Some(&a), &["remote", "add", "other", "file:///sticky-unexpected-push-test.git"]).unwrap();
        run_git(Some(&a), &["config", "remote.pushDefault", "other"]).unwrap();
        let branch = run_git(Some(&a), &["symbolic-ref", "--short", "HEAD"]).unwrap();
        run_git(Some(&a), &["config", &format!("branch.{branch}.pushRemote"), "other"]).unwrap();
        eng.ensure_cloned(&url).unwrap();
        write_file(&a, "days/2026-09-11.md", "# correct journal\n");
        eng.sync_cycle("sticky@workstation: correct origin").unwrap();
        run_git(Some(&b), &["pull"]).unwrap();
        assert!(b.join("days/2026-09-11.md").exists());

        run_git(Some(&a), &["config", &format!("branch.{branch}.remote"), "other"]).unwrap();
        assert!(eng.ensure_cloned(&url).is_err(), "a branch tracking another remote must fail startup validation");
        assert!(eng.pull().is_err());
    }
}
