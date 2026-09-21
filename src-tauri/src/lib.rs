//! 桌面贴纸待办：命令层与应用装配。
//!
//! 命令分组：
//! - 视图：get_view / get_yesterday_leftovers / sync_status
//! - 编辑（全部带 base_version 防陈旧写）：set_checked / set_category / set_priority /
//!   set_flag / set_status（v0.8 状态机）/ set_blocked / set_content（v0.8 可带子行）/
//!   delete_task / restore_deleted / add_task（v0.8 可带子行）/ copy_leftover_to_today
//! - 同步：sync_now（手动 F20）
//!
//! 后台调度线程（sched.rs）负责首运行克隆、30min tick（脏→自动保存 / 干净→静默
//! pull）、失败重试；退出时 RunEvent::ExitRequested 里做 flush（PRD G19：本地
//! commit 保底）。编辑不即时同步（2026-09-10 用户反馈太慢），点角标可手动触发。

// Keep existing stderr output; mirror scheduler errors to logcat on Android.
macro_rules! log_err {
    ($($arg:tt)*) => {{
        eprintln!($($arg)*);
        #[cfg(target_os = "android")]
        log::error!(target: "sticky", $($arg)*);
    }};
}

pub mod notebook;
pub mod notes;
pub mod parser;
#[cfg(windows)]
pub mod placement;
#[cfg(windows)]
pub mod rollover;
pub mod repo;
pub mod sched;
#[cfg(windows)]
pub mod shell;
pub mod store;
pub mod sync;
mod sync_config;

use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use store::{CmdError, StoreState, StickyStore};

/// 贴纸窗口逻辑宽度（设计方案 §2 布局；tauri.conf.json 同步）。
const STICKY_W: f64 = 372.0;

/// 命令层视图解析：day/week 按 date 定位日期及所属周，None/空 = 今天。
fn view_kind(word: &str, date: Option<&str>) -> Result<repo::FileKind, CmdError> {
    store::resolve_kind(word, date)
}

#[cfg(windows)]
fn sticky_hwnd(app: &tauri::AppHandle) -> Option<windows_sys::Win32::Foundation::HWND> {
    let w = app.get_webview_window("sticky")?;
    let h = w.hwnd().ok()?;
    Some(h.0)
}

/// 沉回桌面层（前端 Esc / 5s 无操作调用）。
#[tauri::command]
fn shell_sink(app: tauri::AppHandle) -> Result<(), CmdError> {
    #[cfg(windows)]
    if let Some(h) = sticky_hwnd(&app) {
        shell::sink(h);
    }
    Ok(())
}

/// 内容高度自适应：前端 ResizeObserver 量出文档高，窗口等高（宽固定 372）。
/// Windows 走 placement：底边吸附时向上生长（单次 SetWindowPos，不闪两帧）。
#[tauri::command]
fn set_body_height(height: f64, app: tauri::AppHandle) -> Result<(), CmdError> {
    let w = app
        .get_webview_window("sticky")
        .ok_or_else(|| CmdError::internal("sticky 窗口不存在"))?;
    let h = height.clamp(200.0, 1600.0);
    #[cfg(windows)]
    if let Ok(hwnd) = w.hwnd() {
        placement::resize_anchored(hwnd.0, STICKY_W, h);
        return Ok(());
    }
    w.set_size(tauri::LogicalSize::new(STICKY_W, h))
        .map_err(|e| CmdError::internal(format!("调整窗口尺寸失败: {e}")))?;
    Ok(())
}

#[tauri::command]
fn get_view(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    state: tauri::State<StoreState>,
) -> Result<store::ViewDto, CmdError> {
    state.get_view(&notebook_id, view_kind(&kind, date.as_deref())?)
}

#[tauri::command]
fn get_notes(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    state: tauri::State<StoreState>,
) -> Result<store::NotesDto, CmdError> {
    state.get_notes(&notebook_id, view_kind(&kind, date.as_deref())?)
}

#[tauri::command]
fn save_note(
    notebook_id: String,
    date: Option<String>,
    content: String,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    let repo::FileKind::Day(day) = store::resolve_kind("day", date.as_deref())? else {
        return Err(CmdError::bad_request("应为日速记"));
    };
    state.save_note(&notebook_id, day, &content, parse_bv(&base_version)?)
}

#[tauri::command]
fn get_yesterday_leftovers(
    notebook_id: String,
    state: tauri::State<StoreState>,
) -> Result<store::LeftoversDto, CmdError> {
    let today = chrono::Local::now().date_naive();
    state.get_yesterday_leftovers(&notebook_id, today)
}

/// base_version 跨界为字符串（见 store.rs u64_str 注释），命令层解析。
fn parse_bv(s: &str) -> Result<u64, CmdError> {
    s.parse().map_err(|_| CmdError::bad_request("base_version 必须是数字字符串"))
}

#[tauri::command]
fn copy_leftover_to_today(
    notebook_id: String,
    line_idx: usize,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::AddResult, CmdError> {
    let today = chrono::Local::now().date_naive();
    state.copy_leftover_to_today(&notebook_id, today, line_idx, parse_bv(&base_version)?)
}

#[tauri::command]
fn set_checked(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    checked: bool,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    state.set_checked(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, checked, parse_bv(&base_version)?)
}

#[tauri::command]
fn set_category(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    category: Option<String>,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    let cat = store::parse_category(category.as_deref())?;
    state.set_category(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, cat, parse_bv(&base_version)?)
}

#[tauri::command]
fn set_priority(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    priority: Option<String>,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    let prio = store::parse_priority(priority.as_deref())?;
    state.set_priority(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, prio, parse_bv(&base_version)?)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn set_flag(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    flag: String,
    on: bool,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    let f = store::parse_flag(&flag)?;
    state.set_flag(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, f, on, parse_bv(&base_version)?)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn set_content(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    content: String,
    // v0.8 多行编辑：null=保留旧子行 / []=清空 / 非空=整体替换（两空格结构缩进落盘）
    sub_lines: Option<Vec<String>>,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    parser::validate_user_text(&content).map_err(CmdError::bad_request)?;
    if let Some(subs) = sub_lines.as_deref() {
        parser::validate_sub_lines(subs).map_err(CmdError::bad_request)?;
    }
    state.set_content(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, &content, sub_lines, parse_bv(&base_version)?)
}

/// v0.8 状态机：target ∈ doing|paused|done（todo 拒绝——一次性初始态）。
#[tauri::command]
fn set_status(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    target: String,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    let target = store::parse_status(&target)?;
    state.set_task_status(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, target, parse_bv(&base_version)?)
}

#[tauri::command]
fn delete_task(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::DeleteResult, CmdError> {
    state.delete_task(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, parse_bv(&base_version)?)
}

/// 撤销删除：原样插回 delete_task 返回的行。
#[tauri::command]
fn restore_deleted(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    removed: Vec<String>,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    parser::validate_restore_lines(&removed).map_err(CmdError::bad_request)?;
    state.restore_deleted(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, removed, parse_bv(&base_version)?)
}

/// `#blocked` 写入；reason=None 移除（PRD F7）。
#[tauri::command]
fn set_blocked(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    reason: Option<String>,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    if let Some(r) = reason.as_deref() {
        parser::validate_user_text(r).map_err(CmdError::bad_request)?;
    }
    state.set_blocked(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, reason.as_deref(), parse_bv(&base_version)?)
}

/// 新任务默认 P1（PRD G4）；分类缺省 = 未分类；可选子行（v0.8 多行录入）。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn add_task(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    category: Option<String>,
    priority: Option<String>,
    text: String,
    sub_lines: Option<Vec<String>>,
    base_version: String,
    state: tauri::State<StoreState>,
) -> Result<store::AddResult, CmdError> {
    let cat = store::parse_category(category.as_deref())?
        .unwrap_or(parser::Category::Uncategorized);
    let prio = store::parse_priority(priority.as_deref())?.unwrap_or(parser::Priority::P1);
    parser::validate_user_text(&text).map_err(CmdError::bad_request)?;
    let subs = sub_lines.unwrap_or_default();
    parser::validate_sub_lines(&subs).map_err(CmdError::bad_request)?;
    state.add_task(&notebook_id, &view_kind(&kind, date.as_deref())?, cat, prio, &text, &subs, parse_bv(&base_version)?)
}

#[tauri::command]
fn list_notebooks(state: tauri::State<StoreState>) -> Result<Vec<notebook::NotebookDto>, CmdError> {
    state.list_notebooks()
}

#[tauri::command]
fn create_notebook(name: String, state: tauri::State<StoreState>) -> Result<notebook::NotebookDto, CmdError> {
    state.save_notebook(None, &name, 0)
}

#[tauri::command]
fn rename_notebook(notebook_id: String, name: String, base_version: String, state: tauri::State<StoreState>) -> Result<notebook::NotebookDto, CmdError> {
    state.save_notebook(Some(&notebook_id), &name, parse_bv(&base_version)?)
}

#[tauri::command]
fn sync_now(state: tauri::State<StoreState>) -> Result<(), CmdError> {
    state.request_sync_now();
    Ok(())
}

#[tauri::command]
fn sync_status(state: tauri::State<StoreState>) -> store::SyncUiState {
    state.status()
}

/// 本机提交身份（页脚右下角展示，如 sticky@office）。
#[tauri::command]
fn machine_tag() -> String {
    crate::sync::machine_tag()
}

/// 窗口壳装配：位置恢复/默认右下角（placement）→ 沉回桌面层 → 注册 Alt+S 全局
/// 热键（设计方案 §0）。热键注册失败只记日志不崩（可能被其他程序占用）。
fn setup_shell(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let Some(win) = app.get_webview_window("sticky") else {
        return Err("sticky 窗口未配置".into());
    };

    #[cfg(windows)]
    if let Ok(h) = win.hwnd() {
        placement::init(h.0); // 恢复上次位置（无效则默认工作区右下角贴齐）
        shell::sink_to_bottom(h.0);
        shell::spawn_fullscreen_watch(h.0); // F12：前台全屏自动隐藏
    }

    #[cfg(desktop)]
    register_hotkey(app);

    Ok(())
}

/// Alt+S 全局热键：桌面专属能力（移动端不注册插件，编译期排除）。
#[cfg(desktop)]
fn register_hotkey(app: &tauri::App) {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    let gs = app.global_shortcut();
    if let Err(e) = gs.on_shortcut("alt+s", |app, _sc, event| {
        if event.state != ShortcutState::Pressed {
            return;
        }
        #[cfg(windows)]
        if shell::is_sunk() {
            if let Some(w) = app.get_webview_window("sticky") {
                if let Ok(h) = w.hwnd() {
                    shell::summon(h.0);
                }
                let _ = w.set_focus();
            }
        }
    }) {
        eprintln!("[shell] Alt+S 热键注册失败（可能被占用）: {e}");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("sticky")
            .with_max_level(log::LevelFilter::Info)
            .with_filter(android_logger::FilterBuilder::new().parse("off,sticky=info").build()),
    );
    let store: StoreState = Arc::new(StickyStore::new(store::default_repo_dir()));

    let builder = tauri::Builder::default()
        .manage(store.clone())
        .invoke_handler(tauri::generate_handler![
            list_notebooks,
            create_notebook,
            rename_notebook,
            get_view,
            get_notes,
            save_note,
            get_yesterday_leftovers,
            copy_leftover_to_today,
            set_checked,
            set_category,
            set_priority,
            set_flag,
            set_status,
            set_content,
            delete_task,
            restore_deleted,
            set_blocked,
            add_task,
            sync_now,
            sync_config::get_sync_config,
            sync_config::set_sync_config,
            sync_status,
            machine_tag,
            shell_sink,
            set_body_height
        ])
        // 拖动落定：Moved 只记账，placement 监视线程防抖后吸附+落盘
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Moved(_) = event {
                if window.label() == "sticky" {
                    #[cfg(windows)]
                    if let Ok(h) = window.hwnd() {
                        placement::note_moved(h.0);
                    }
                }
            }
        })
        .setup(setup_shell);

    // 同机单实例（桌面专属）：二次启动的回调在既有实例里聚焦窗口，新进程随即退出。
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        use tauri::Manager;
        if let Some(win) = app.get_webview_window("sticky") {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }));

    // 全局热键插件为桌面专属（见 register_hotkey）；移动端不挂载。
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());

    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // 后台调度：事件桥接到 WebView（sync-status / view-changed / repo-ready）
    let emit_handle = app.handle().clone();
    let sched = sched::spawn(
        store.clone(),
        sched::SchedTiming::default(),
        Arc::new(move |event: &str, payload: &str| {
            use tauri::Emitter;
            // EmitFn 约定 payload 是 JSON 字符串；直接 emit 字符串会让 WebView
            // 收到 string 而非对象（payload.kind 恒 undefined，角标永远停在默认
            // 文案——冲突 E2E 抓到：Conflict 状态 UI 不可见）。先解析成 Value 再发。
            let value: serde_json::Value =
                serde_json::from_str(payload).unwrap_or_else(|_| serde_json::Value::String(payload.into()));
            let _ = emit_handle.emit(event, value);
        }),
    );

    // 退出 flush：防止事件循环直接结束杀掉后台线程（PRD G19）
    let mut flushed = false;
    let mut sched = Some(sched);
    app.run(move |app_handle, event| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            if code.is_none() && !flushed {
                flushed = true;
                api.prevent_exit();
                app_handle.state::<StoreState>().detach_scheduler();
                if let Some(s) = sched.take() {
                    s.stop(Duration::from_secs(15));
                }
                app_handle.exit(0);
            }
        }
    });
}
