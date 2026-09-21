//! 状态层：Tauri 命令与 parser / sync 之间的桥。
//!
//! **base_version 防陈旧写**：前端拿到的每个视图带 `base_version` = 文件内容哈希
//! （文件不存在 = 0）。任何编辑都要回传它；后端在持锁的读-改-写里重算哈希比对，
//! 不一致说明磁盘版本已被 pull / 其他端更新 → 拒绝写入（`stale`），前端重新拉视图。
//!
//! **io 锁**：`读-改-写` 与 git pull（重写工作区文件）互斥，消除竞态窗口。
//! v1 为正确性优先，锁覆盖整个同步周期（秒级）；同步只在 30min tick / 手动 / 退出发生。
//!
//! **仓库硬约束**：不写 `_current.md`（归外部例程维护）、
//! `#overdue` 只读（不可设置不可清除）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use chrono::{Datelike, Duration, Local};
use crate::notebook::{self, NotebookDto};
use crate::notes;

use crate::parser::{Category, Flag, Priority, TodoFile};
use crate::repo::{FileKind, DAY_TASK_SECTION};
use crate::sched::SchedMsg;
use crate::sync::{SyncEngine, SyncError};

/// 默认同步仓（占位地址：首次启动请在「同步设置」或 STICKY_REPO_URL 指向你自己的仓库）。
pub const REPO_URL: &str = "https://github.com/your-account/sticky-sync.git";

/// 环境变量覆盖（冒烟测试 / 多机部署用）。
pub fn repo_url() -> String {
    std::env::var("STICKY_REPO_URL").unwrap_or_else(|_| REPO_URL.to_string())
}

pub fn default_repo_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("STICKY_REPO_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    // 安卓没有 LOCALAPPDATA，temp_dir 也不可写：用应用私有 files 目录
    // （硬编码包名；P5.2 存储重构时换 tauri path API 解析）。
    #[cfg(target_os = "android")]
    {
        return PathBuf::from("/data/data/com.firoyu.sticky/files/sticky/sticky-sync");
    }
    #[cfg(not(target_os = "android"))]
    if let Some(v) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(v).join("sticky").join("sticky-sync");
    }
    std::env::temp_dir().join("sticky-sync")
}

/// 注意：DefaultHasher 的算法跨 rustc 版本不保证稳定，但 base_version 只在
/// 同一进程会话内往返（前端每次启动都重新 get_view），不受影响。
pub fn hash_text(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// 命令层错误：code 供前端分支处理，message 供展示。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CmdError {
    pub code: &'static str,
    pub message: String,
}

impl CmdError {
    pub(crate) fn stale() -> Self {
        CmdError { code: "stale", message: "文件已被远端更新，请刷新后重试".into() }
    }
    fn repo_not_ready() -> Self {
        CmdError { code: "repo_not_ready", message: "仓库尚未克隆完成".into() }
    }
    pub(crate) fn bad_request(m: impl Into<String>) -> Self {
        CmdError { code: "bad_request", message: m.into() }
    }
    pub(crate) fn internal(m: impl Into<String>) -> Self {
        CmdError { code: "internal", message: m.into() }
    }
}

impl From<SyncError> for CmdError {
    fn from(e: SyncError) -> Self {
        CmdError { code: "git", message: e.to_string() }
    }
}

/// 角标状态（PRD F10）：前端据此渲染同步指示。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SyncUiState {
    Idle,
    Syncing,
    /// 断网 / push 失败：本地提交积压，等自动补推
    Offline { unpushed: usize },
    /// rebase 真冲突：本地修改保留，等人工处理（重试 / 打开仓库文件夹）
    Conflict { message: String },
    Error { message: String },
}


/// base_version（u64 内容哈希）跨界序列化为字符串：JSON Number 经 JS 往返
/// 丢失精度（> 2^53），数值回传后必然 stale（2026-09-10 实测：勾选静默丢失）。
fn u64_str<S: serde::Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

#[derive(Debug, serde::Serialize)]
pub struct ViewDto {
    pub notebook_id: String,
    pub sources: Vec<TaskSource>,
    /// "day" | "week"
    pub kind: String,
    /// 当前所选日期，YYYY-MM-DD
    pub date: String,
    pub rel_path: String,
    pub exists: bool,
    /// 文件不存在 = 0
    #[serde(serialize_with = "u64_str")]
    pub base_version: u64,
    pub tasks: Vec<ViewTask>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskSource {
    pub kind: String,
    pub date: String,
    pub rel_path: String,
    #[serde(serialize_with = "u64_str")]
    pub base_version: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct NotesDayDto {
    pub date: String,
    pub weekday: String,
    pub content: String,
    #[serde(serialize_with = "u64_str")]
    pub base_version: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct NotesDto {
    pub kind: String,
    pub date: String,
    pub days: Vec<NotesDayDto>,
}

#[derive(Debug, serde::Serialize)]
pub struct ViewTask {
    #[serde(flatten)]
    pub task: crate::parser::TaskView,
    pub source: TaskSource,
}

impl std::ops::Deref for ViewTask {
    type Target = crate::parser::TaskView;
    fn deref(&self) -> &Self::Target { &self.task }
}

#[derive(Debug, serde::Serialize)]
pub struct EditResult {
    #[serde(serialize_with = "u64_str")]
    pub base_version: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct AddResult {
    #[serde(serialize_with = "u64_str")]
    pub base_version: u64,
    pub line_idx: usize,
}

/// 删除结果：带回被删原文与行号，前端 3s 撤销气泡用其精确恢复。
#[derive(Debug, serde::Serialize)]
pub struct DeleteResult {
    #[serde(serialize_with = "u64_str")]
    pub base_version: u64,
    pub line_idx: usize,
    /// 被删的原始行（任务行 + 缩进子行），原样插回即恢复
    pub removed: Vec<String>,
}

/// F14「昨天还有 N 项」的只读条目。
#[derive(Debug, serde::Serialize)]
pub struct LeftoverDto {
    pub line_idx: usize,
    pub content: String,
    pub priority: Option<Priority>,
    pub category: Category,
}

#[derive(Debug, serde::Serialize)]
pub struct LeftoversDto {
    pub date: String,
    pub count: usize,
    pub tasks: Vec<LeftoverDto>,
}

pub struct StickyStore {
    engine: SyncEngine,
    /// 串行化「读-改-写」与「同步周期」（pull 会重写工作区）
    io: Mutex<()>,
    status: Mutex<SyncUiState>,
    pending_edits: AtomicUsize,
    tx: Mutex<Option<Sender<SchedMsg>>>,
    /// 仓库就绪：.git 目录在 clone 中途就会出现（checkout 前），不能作就绪判据；
    /// 首运行由调度线程克隆完成后置位，避免 mid-clone 编辑被 checkout 覆盖丢失。
    repo_ready: AtomicBool,
}

impl StickyStore {
    pub fn new(repo_dir: impl Into<PathBuf>) -> Self {
        let repo_dir = repo_dir.into();
        let ready = repo_dir.join(".git").exists();
        StickyStore {
            engine: SyncEngine::new(repo_dir),
            io: Mutex::new(()),
            status: Mutex::new(SyncUiState::Idle),
            pending_edits: AtomicUsize::new(0),
            tx: Mutex::new(None),
            repo_ready: AtomicBool::new(ready),
        }
    }

    pub fn engine(&self) -> &SyncEngine {
        &self.engine
    }

    pub fn status(&self) -> SyncUiState {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_status(&self, s: SyncUiState) {
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = s;
    }

    pub fn attach_scheduler(&self, tx: Sender<SchedMsg>) {
        *self.tx.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
    }

    pub fn detach_scheduler(&self) {
        *self.tx.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn notify(&self, msg: SchedMsg) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            let _ = tx.send(msg);
        }
    }

    pub fn take_pending_edits(&self) -> usize {
        self.pending_edits.swap(0, Ordering::Relaxed)
    }

    /// 只读查看待同步编辑数（不取走；调度层据此判断是否需要补一轮同步）。
    pub fn pending_edit_count(&self) -> usize {
        self.pending_edits.load(Ordering::Relaxed)
    }

    /// 非命令层的批量写入（跨日流转）完成后计入待同步编辑数。
    #[cfg(windows)]
    pub(crate) fn add_pending_edits(&self, n: usize) {
        self.pending_edits.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn io_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.io.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn io_mutex(&self) -> &Mutex<()> {
        &self.io
    }

    // read_scope 在桌面免锁：拉取重写工作区瞬间的撕裂读由 view-changed 指纹刷新自愈。
    // 写路径仍持锁，并由 base_version 哈希校验兜底；安卓保持读锁。
    fn read_scope(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        #[cfg(target_os = "android")]
        { return Some(self.io.lock().unwrap_or_else(|p| p.into_inner())); }
        #[cfg(not(target_os = "android"))]
        { None }
    }

    pub(crate) fn ensure_repo_ready(&self) -> Result<(), CmdError> {
        if !self.repo_ready.load(Ordering::Acquire) {
            return Err(CmdError::repo_not_ready());
        }
        Ok(())
    }

    /// 调度线程在克隆真正完成后（或启动时确认既有仓库）置位。
    pub fn mark_repo_ready(&self) {
        self.repo_ready.store(true, Ordering::Release);
    }

    /// 调度器启动时先关闭命令入口，等待本机仓库目标校验完成。
    pub(crate) fn mark_repo_not_ready(&self) {
        self.repo_ready.store(false, Ordering::Release);
    }

    fn read_file(&self, notebook: &str, kind: &FileKind) -> Result<Option<String>, CmdError> {
        let path = kind.abs_path(&notebook::root(self.engine.repo_dir(), notebook)?);
        match std::fs::read_to_string(&path) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CmdError::internal(format!("读取 {} 失败: {e}", kind.rel_path()))),
        }
    }

    /// 缺文件时在内存套模板；编辑验证成功后才创建文件（PRD G5）。
    pub(crate) fn materialize_template(&self, kind: &FileKind) -> Result<String, CmdError> {
        let tpl_path = self.engine.repo_dir().join(kind.template_rel());
        let tpl = std::fs::read_to_string(&tpl_path).map_err(|e| {
            CmdError::internal(format!("模板 {} 缺失: {e}", kind.template_rel()))
        })?;
        Ok(kind.fill_template(&tpl))
    }

    pub fn list_notebooks(&self) -> Result<Vec<NotebookDto>, CmdError> {
        self.ensure_repo_ready()?;
        let _g = self.read_scope();
        notebook::list(self.engine.repo_dir())
    }

    pub fn save_notebook(&self, id: Option<&str>, name: &str, base_version: u64) -> Result<NotebookDto, CmdError> {
        self.ensure_repo_ready()?;
        let _g = self.io_lock();
        let book = notebook::save(self.engine.repo_dir(), id, name, base_version)?;
        self.pending_edits.fetch_add(1, Ordering::Relaxed);
        Ok(book)
    }

    pub fn get_view(&self, notebook: &str, kind: FileKind) -> Result<ViewDto, CmdError> {
        self.ensure_repo_ready()?;
        let _g = self.read_scope();
        let mut kinds = vec![kind];
        if let FileKind::Week(date) = kind {
            let monday = date.checked_sub_signed(Duration::days(i64::from(date.weekday().num_days_from_monday())))
                .ok_or_else(|| CmdError::bad_request("日期超出范围"))?;
            for offset in 0..7 {
                kinds.push(FileKind::Day(monday.checked_add_signed(Duration::days(offset))
                    .ok_or_else(|| CmdError::bad_request("日期超出范围"))?));
            }
        }
        let mut sources = Vec::new();
        let mut tasks = Vec::new();
        let mut exists = false;
        for source_kind in kinds {
            let text = self.read_file(notebook, &source_kind)?;
            exists |= text.is_some();
            let source = TaskSource {
                kind: match source_kind { FileKind::Day(_) => "day", FileKind::Week(_) => "week" }.into(),
                date: match source_kind { FileKind::Day(d) | FileKind::Week(d) => crate::repo::fmt_date(d) },
                rel_path: if notebook == notebook::DEFAULT_NOTEBOOK { source_kind.rel_path() }
                    else { format!("notebooks/{notebook}/{}", source_kind.rel_path()) },
                base_version: text.as_deref().map(hash_text).unwrap_or(0),
            };
            if let Some(text) = text {
                tasks.extend(TodoFile::parse(&text).tasks.into_iter().map(|task| ViewTask { task, source: source.clone() }));
            }
            sources.push(source);
        }
        Ok(ViewDto {
            notebook_id: notebook.into(),
            kind: sources[0].kind.clone(),
            date: sources[0].date.clone(),
            rel_path: sources[0].rel_path.clone(),
            exists,
            base_version: sources[0].base_version,
            sources,
            tasks,
        })
    }

    pub fn get_notes(&self, notebook: &str, kind: FileKind) -> Result<NotesDto, CmdError> {
        self.ensure_repo_ready()?;
        let _g = self.read_scope();
        let root = notebook::root(self.engine.repo_dir(), notebook)?;
        let today = Local::now().date_naive();
        let date = match kind { FileKind::Day(d) | FileKind::Week(d) => d };
        let candidates = match kind {
            FileKind::Day(d) => vec![d],
            FileKind::Week(d) => {
                let monday = d.checked_sub_signed(Duration::days(i64::from(d.weekday().num_days_from_monday())))
                    .ok_or_else(|| CmdError::bad_request("日期超出范围"))?;
                let days = notes::week_days(monday, today);
                if days.len() != 7 { return Err(CmdError::bad_request("日期超出范围")); }
                days
            }
        };
        let mut existing = Vec::new();
        let mut days = Vec::new();
        for d in candidates {
            let rel = notes::rel_path(d);
            let text = match std::fs::read_to_string(root.join(&rel)) {
                Ok(t) => Some(t),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(CmdError::internal(format!("读取 {rel} 失败: {e}"))),
            };
            let base_version = text.as_deref().map(hash_text).unwrap_or(0);
            let content = text.unwrap_or_default();
            existing.push((d, content.clone()));
            days.push(NotesDayDto {
                date: crate::repo::fmt_date(d), weekday: notes::weekday_full(d.weekday()).into(),
                content, base_version,
            });
        }
        if matches!(kind, FileKind::Week(_)) {
            let included = notes::days_to_include(&existing, today);
            days = days.into_iter().zip(existing).filter(|(_, (d, _))| included.contains(d))
                .map(|(day, _)| day).collect();
        }
        Ok(NotesDto {
            kind: match kind { FileKind::Day(_) => "day", FileKind::Week(_) => "week" }.into(),
            date: crate::repo::fmt_date(date), days,
        })
    }

    pub fn save_note(&self, notebook: &str, date: chrono::NaiveDate, content: &str, base_version: u64) -> Result<EditResult, CmdError> {
        let content = notes::validate_and_normalize(content).map_err(CmdError::bad_request)?;
        let _g = self.io_lock();
        self.ensure_repo_ready()?;
        let rel = notes::rel_path(date);
        let path = notebook::root(self.engine.repo_dir(), notebook)?.join(&rel);
        let current = match std::fs::read_to_string(&path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(CmdError::internal(format!("读取 {rel} 失败: {e}"))),
        };
        if current.as_deref().map(hash_text).unwrap_or(0) != base_version {
            return Err(CmdError::stale());
        }
        let version = if content.trim().is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(CmdError::internal(format!("删除 {rel} 失败: {e}"))),
            }
            0
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CmdError::internal(format!("建目录失败: {e}")))?;
            }
            notebook::atomic_write(&path, &content)?;
            hash_text(&content)
        };
        self.pending_edits.fetch_add(1, Ordering::Relaxed);
        Ok(EditResult { base_version: version })
    }

    /// 核心：持锁读-改-写 + base_version 校验 + pending_edits 计数（供 commit message 汇总）。
    fn edit<F>(&self, notebook: &str, kind: &FileKind, base_version: u64, f: F) -> Result<u64, CmdError>
    where
        F: FnOnce(&mut TodoFile) -> Result<(), String>,
    {
        self.ensure_repo_ready()?;
        let _g = self.io_lock();
        let (text, created_now) = match self.read_file(notebook, kind)? {
            Some(t) => {
                if base_version == 0 {
                    // 前端以为是新建，但文件已存在（别端先建了）
                    return Err(CmdError::stale());
                }
                (t, false)
            }
            None => {
                if base_version != 0 {
                    // 前端基于旧内容，但文件已消失
                    return Err(CmdError::stale());
                }
                // 刚套模板建出的文件内容当然不等于 base_version(0)，跳过哈希校验
                (self.materialize_template(kind)?, true)
            }
        };
        if !created_now && hash_text(&text) != base_version {
            return Err(CmdError::stale());
        }
        let mut file = TodoFile::parse(&text);
        f(&mut file).map_err(CmdError::bad_request)?;
        let out = file.serialize();
        // 空操作短路（v0.8）：闭包幂等成功但字节无变化（如同态 set_status、
        // 重复 remove_flag）时不写盘、不计 pending，版本原样返回。
        if !created_now && out == text {
            return Ok(base_version);
        }
        let path = kind.abs_path(&notebook::root(self.engine.repo_dir(), notebook)?);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CmdError::internal(format!("建目录失败: {e}")))?;
        }
        notebook::atomic_write(&path, &out)?;
        self.pending_edits.fetch_add(1, Ordering::Relaxed);
        Ok(hash_text(&out))
    }

    /// 服务端时钟：一次状态手术只读一次（手术内结算与写 ts 用同一值）。
    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// v0.8 状态机入口：target ∈ doing|paused|done（todo 由 parse_status 拒绝）。
    /// 结算/续计/复活的字节手术全部在 parser::set_status_at 内完成。
    pub fn set_task_status(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        target: crate::parser::TaskStatus,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        let now = Self::unix_now();
        let bv = self.edit(notebook, kind, base_version, move |f| {
            f.set_status_at(line_idx, target, now)
        })?;
        Ok(EditResult { base_version: bv })
    }

    /// 旧勾选命令的 v0.8.1 适配：勾 = set_status(done)（Doing 结算冻结）；
    /// 取消勾选且当前 Done = set_status(paused)，保留冻结计时；其余（非 Done 取消勾选）无操作。
    pub fn set_checked(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        checked: bool,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        let now = Self::unix_now();
        let bv = self.edit(notebook, kind, base_version, move |f| {
            let cur = f.status_at(line_idx)?;
            match (checked, cur) {
                (true, _) => f.set_status_at(line_idx, crate::parser::TaskStatus::Done, now),
                (false, crate::parser::TaskStatus::Done) => {
                    f.set_status_at(line_idx, crate::parser::TaskStatus::Paused, now)
                }
                (false, _) => Ok(()),
            }
        })?;
        Ok(EditResult { base_version: bv })
    }

    pub fn set_category(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        cat: Option<Category>,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        let bv = self.edit(notebook, kind, base_version, |f| f.set_category(line_idx, cat))?;
        Ok(EditResult { base_version: bv })
    }

    pub fn set_priority(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        prio: Option<Priority>,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        let bv = self.edit(notebook, kind, base_version, |f| f.set_priority(line_idx, prio))?;
        Ok(EditResult { base_version: bv })
    }

    /// 状态标签。`#overdue` 只读（PRD D4）——不可设置、不可清除。
    /// v0.8 适配：doing 的开/关映射到状态机 Doing/Paused（含遗留无 ts 补启），
    /// 其余情况无操作（保持旧幂等语义，不再直接增删标签）。
    pub fn set_flag(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        flag: Flag,
        on: bool,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        if flag == Flag::Overdue {
            return Err(CmdError::bad_request("#overdue 只读：由外部例程维护"));
        }
        let now = Self::unix_now();
        let bv = self.edit(notebook, kind, base_version, move |f| {
            let cur = f.status_at(line_idx)?;
            match (on, cur) {
                (true, _) => f.set_status_at(line_idx, crate::parser::TaskStatus::Doing, now),
                (false, crate::parser::TaskStatus::Doing) => {
                    f.set_status_at(line_idx, crate::parser::TaskStatus::Paused, now)
                }
                (false, _) => Ok(()),
            }
        })?;
        Ok(EditResult { base_version: bv })
    }

    /// 内容替换（可选子行三态：None 保留旧子行 / Some([]) 清空 / Some(vec) 替换）。
    pub fn set_content(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        content: &str,
        sub_lines: Option<Vec<String>>,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        let bv = self.edit(notebook, kind, base_version, move |f| {
            f.set_content(line_idx, content)?;
            // 行内手术先行（行号不变），子行整块替换随后（行数变化触发全量重建）
            if let Some(subs) = sub_lines.as_deref() {
                f.replace_sub_lines(line_idx, subs)?;
            }
            Ok(())
        })?;
        Ok(EditResult { base_version: bv })
    }

    pub fn delete_task(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        base_version: u64,
    ) -> Result<DeleteResult, CmdError> {
        let mut removed = Vec::new();
        let bv = self.edit(notebook, kind, base_version, |f| {
            removed = f.delete_task(line_idx)?;
            Ok(())
        })?;
        Ok(DeleteResult { base_version: bv, line_idx, removed })
    }

    /// 撤销删除：把 delete_task 返回的原始行原样插回原行号。
    pub fn restore_deleted(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        removed: Vec<String>,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        if removed.is_empty() {
            return Err(CmdError::bad_request("没有可恢复的行"));
        }
        let bv = self.edit(notebook, kind, base_version, |f| {
            f.insert_lines_at(line_idx, removed.clone());
            Ok(())
        })?;
        Ok(EditResult { base_version: bv })
    }

    /// `#blocked` 写入（PRD F7）；None = 移除（连带原因）。
    pub fn set_blocked(
        &self, notebook: &str,
        kind: &FileKind,
        line_idx: usize,
        reason: Option<&str>,
        base_version: u64,
    ) -> Result<EditResult, CmdError> {
        let bv = self.edit(notebook, kind, base_version, |f| f.set_blocked(line_idx, reason))?;
        Ok(EditResult { base_version: bv })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_task(
        &self, notebook: &str,
        kind: &FileKind,
        cat: Category,
        prio: Priority,
        text: &str,
        subs: &[String],
        base_version: u64,
    ) -> Result<AddResult, CmdError> {
        let section = match kind {
            FileKind::Day(_) => DAY_TASK_SECTION,
            FileKind::Week(_) => crate::repo::WEEK_TASK_SECTION,
        };
        let mut line = usize::MAX;
        let bv = self.edit(notebook, kind, base_version, |f| {
            line = f.add_task(section, cat, prio, text, subs)?;
            Ok(())
        })?;
        Ok(AddResult { base_version: bv, line_idx: line })
    }

    /// F14：昨天未完成条目（只读，含同文件内去重——设计方案 N1 保守版）。
    pub fn get_yesterday_leftovers(&self, notebook: &str, today: chrono::NaiveDate) -> Result<LeftoversDto, CmdError> {
        self.ensure_repo_ready()?;
        let yesterday = FileKind::Day(today - Duration::days(1));
        let _g = self.read_scope();
        let text = self.read_file(notebook, &yesterday)?;
        let mut seen = std::collections::HashSet::new();
        let mut tasks = Vec::new();
        if let Some(t) = text {
            for v in TodoFile::parse(&t).tasks {
                if v.checked || !seen.insert(v.content.clone()) {
                    continue;
                }
                tasks.push(LeftoverDto {
                    line_idx: v.line_idx,
                    content: v.content,
                    priority: v.priority,
                    category: v.category,
                });
            }
        }
        let count = tasks.len();
        Ok(LeftoversDto { date: yesterday.rel_path(), count, tasks })
    }

    /// F14「复制到今天」：复制 文本+优先级+分类+子行（D5——#overdue 等状态是
    /// 旧日期的，不带；v0.8 起子行随主行一起复制）。昨日原条目不动。
    /// base_version 校验的是**今天**的文件。
    pub fn copy_leftover_to_today(
        &self, notebook: &str,
        today: chrono::NaiveDate,
        line_idx: usize,
        base_version: u64,
    ) -> Result<AddResult, CmdError> {
        let yesterday = FileKind::Day(today - Duration::days(1));
        let (content, prio, cat, subs) = {
            let _g = self.io_lock();
            let text = self
                .read_file(notebook, &yesterday)?
                .ok_or_else(|| CmdError::bad_request("昨日文件不存在"))?;
            let f = TodoFile::parse(&text);
            let t = f
                .tasks
                .iter()
                .find(|t| t.line_idx == line_idx)
                .ok_or_else(|| CmdError::bad_request("line 不是昨日任务行"))?;
            (t.content.clone(), t.priority, t.category, t.sub_lines.clone())
        };
        let today_kind = FileKind::Day(today);
        let mut line = usize::MAX;
        let bv = self.edit(notebook, &today_kind, base_version, |f| {
            line = f.add_task(DAY_TASK_SECTION, cat, prio.unwrap_or(Priority::P1), &content, &subs)?;
            if prio.is_none() {
                f.set_priority(line, None)?;
            }
            Ok(())
        })?;
        Ok(AddResult { base_version: bv, line_idx: line })
    }

    /// 手动同步（F20）：调度器立即走完整周期。
    pub fn request_sync_now(&self) {
        self.notify(SchedMsg::SyncNow);
    }

    /// Compare journal files around a sync in every notebook, including
    /// historical days currently open in the UI.
    pub fn watched_fingerprints(&self, _today: chrono::NaiveDate) -> Vec<(String, u64)> {
        fn visit(root: &std::path::Path, path: &std::path::Path, out: &mut Vec<(String, u64)>) {
            let Ok(entries) = std::fs::read_dir(path) else { return; };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else { continue; };
                if kind.is_dir() { visit(root, &path, out); }
                else if kind.is_file() && (path.extension().is_some_and(|e| e == "md") || entry.file_name() == "notebook.json") {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        out.push((path.strip_prefix(root).unwrap().to_string_lossy().into_owned(), hash_text(&text)));
                    }
                }
            }
        }
        let root = self.engine.repo_dir();
        let mut out = Vec::new();
        for dir in ["days", "weeks", "notes", "notebooks"] { visit(root, &root.join(dir), &mut out); }
        out.sort();
        out
    }

}

/// 视图 → 文件（"day" | "week"）。按 date（YYYY-MM-DD）定位日或所属周；
/// 缺省/空 = 今天。
pub fn resolve_kind(word: &str, date: Option<&str>) -> Result<FileKind, CmdError> {
    let day = match date {
        None | Some("") => Local::now().date_naive(),
        Some(s) => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| CmdError::bad_request("date 应为 YYYY-MM-DD"))?,
    };
    match word {
        "day" => Ok(FileKind::Day(day)),
        "week" => Ok(FileKind::Week(day)),
        _ => Err(CmdError::bad_request("未知视图类型，应为 day | week")),
    }
}

pub fn parse_category(word: Option<&str>) -> Result<Option<Category>, CmdError> {
    match word {
        None | Some("") => Ok(None),
        Some("work") => Ok(Some(Category::Work)),
        Some("personal") => Ok(Some(Category::Personal)),
        Some(other) => Err(CmdError::bad_request(format!("未知分类: {other}（应为 work | personal | null）"))),
    }
}

pub fn parse_priority(word: Option<&str>) -> Result<Option<Priority>, CmdError> {
    match word {
        None | Some("") => Ok(None),
        Some(s) => match s {
            "P0" => Ok(Some(Priority::P0)),
            "P1" => Ok(Some(Priority::P1)),
            "P2" => Ok(Some(Priority::P2)),
            "P3" => Ok(Some(Priority::P3)),
            other => Err(CmdError::bad_request(format!("未知优先级: {other}（应为 P0-P3 | null）"))),
        },
    }
}

pub fn parse_flag(word: &str) -> Result<Flag, CmdError> {
    match word {
        "doing" => Ok(Flag::Doing),
        "overdue" => Ok(Flag::Overdue),
        other => Err(CmdError::bad_request(format!("未知标签: {other}（应为 doing）"))),
    }
}

/// v0.8 set_status 的目标态：doing | paused | done。todo 是一次性初始态，
/// 一旦离开永不回归（任何命令不可选）。
pub fn parse_status(word: &str) -> Result<crate::parser::TaskStatus, CmdError> {
    match word {
        "doing" => Ok(crate::parser::TaskStatus::Doing),
        "paused" => Ok(crate::parser::TaskStatus::Paused),
        "done" => Ok(crate::parser::TaskStatus::Done),
        "todo" => Err(CmdError::bad_request("待开始是一次性初始状态，一旦离开不可恢复")),
        other => Err(CmdError::bad_request(format!("未知状态: {other}（应为 doing | paused | done）"))),
    }
}

/// Tauri 管理的状态句柄。
pub type StoreState = Arc<StickyStore>;
