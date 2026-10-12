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

pub mod merge_resolve;
pub mod notebook;
pub mod notes;
pub mod stickies;
pub mod parser;
#[cfg(windows)]
pub mod edge_hide;
#[cfg(windows)]
pub mod placement;
#[cfg(target_os = "macos")]
mod position;
#[cfg(windows)]
pub mod rollover;
#[cfg(windows)]
pub mod update;
pub mod repo;
pub mod sched;
#[cfg(windows)]
pub mod shell;
pub mod store;
pub mod sync;
mod sync_config;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use store::{CmdError, StoreState, StickyStore};

/// 贴纸窗口逻辑宽度（设计方案 §2 布局；tauri.conf.json 同步）。
const STICKY_W: f64 = 372.0;
// Physical pixels throughout, matching Tauri's window and monitor geometry APIs.
static BOARD_WINDOW_ZOOMED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
pub(crate) fn board_window_zoomed() -> bool {
    BOARD_WINDOW_ZOOMED.load(Ordering::SeqCst)
}

fn board_editor_rect(area: tauri::PhysicalRect<i32, u32>) -> tauri::PhysicalRect<i32, u32> {
    const MARGIN: u32 = 28;
    let max_width = ((f64::from(area.size.width) * 0.96).round() as u32).min(area.size.width);
    let margin = MARGIN.min(max_width / 2).min(area.size.height / 2);
    let max_paper_width = max_width - 2 * margin;
    let max_paper_height = area.size.height - 2 * margin;
    let paper_height = ((f64::from(area.size.height) * 0.88).round() as u32)
        .min(max_paper_height)
        .min(max_paper_width.saturating_mul(297) / 210);
    let paper_width = (f64::from(paper_height) * 210.0 / 297.0).round() as u32;
    let width = paper_width + 2 * margin;
    let height = paper_height + 2 * margin;
    tauri::PhysicalRect {
        position: tauri::PhysicalPosition::new(
            i64::from(area.position.x).saturating_add(i64::from(area.size.width - width) / 2) as i32,
            i64::from(area.position.y).saturating_add(i64::from(area.size.height - height) / 2) as i32,
        ),
        size: tauri::PhysicalSize::new(width, height),
    }
}

fn resized_board_editor_rect(
    area: tauri::PhysicalRect<i32, u32>,
    current: tauri::PhysicalRect<i32, u32>,
    requested_paper_width: u32,
) -> tauri::PhysicalRect<i32, u32> {
    const MARGIN: u32 = 28;
    let max_width = area.size.width.saturating_sub(2 * MARGIN).min(
        area.size.height.saturating_sub(2 * MARGIN).saturating_mul(210) / 297,
    );
    let paper_width = requested_paper_width.max(340.min(max_width)).min(max_width);
    let width = paper_width + 2 * MARGIN;
    let height = ((f64::from(paper_width) * 297.0 / 210.0).round() as u32) + 2 * MARGIN;
    let center_x = i64::from(current.position.x) + i64::from(current.size.width / 2);
    let center_y = i64::from(current.position.y) + i64::from(current.size.height / 2);
    let x = (center_x - i64::from(width / 2))
        .clamp(i64::from(area.position.x), i64::from(area.position.x) + i64::from(area.size.width.saturating_sub(width)));
    let y = (center_y - i64::from(height / 2))
        .clamp(i64::from(area.position.y), i64::from(area.position.y) + i64::from(area.size.height.saturating_sub(height)));
    tauri::PhysicalRect {
        position: tauri::PhysicalPosition::new(x as i32, y as i32),
        size: tauri::PhysicalSize::new(width, height),
    }
}

#[tauri::command]
fn resize_board_editor(width: f64, app: tauri::AppHandle) -> Result<(), CmdError> {
    if !width.is_finite() || width <= 0.0 {
        return Err(CmdError::bad_request("invalid board editor width"));
    }
    let editor = app.get_webview_window("board-editor")
        .ok_or_else(|| CmdError::internal("board-editor window does not exist"))?;
    let monitor = editor.current_monitor()
        .map_err(|e| CmdError::internal(format!("read monitor failed: {e}")))?
        .ok_or_else(|| CmdError::internal("current monitor does not exist"))?;
    let current = tauri::PhysicalRect {
        position: editor.outer_position()
            .map_err(|e| CmdError::internal(format!("read board editor position failed: {e}")))?,
        size: editor.outer_size()
            .map_err(|e| CmdError::internal(format!("read board editor size failed: {e}")))?,
    };
    let scale = editor.scale_factor()
        .map_err(|e| CmdError::internal(format!("read board editor scale failed: {e}")))?;
    let rect = resized_board_editor_rect(*monitor.work_area(), current, (width * scale).round() as u32);
    editor.set_size(rect.size)
        .map_err(|e| CmdError::internal(format!("resize board editor failed: {e}")))?;
    editor.set_position(rect.position)
        .map_err(|e| CmdError::internal(format!("position board editor failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod board_editor_rect_tests {
    use super::{board_editor_rect, resized_board_editor_rect};
    use tauri::{PhysicalPosition, PhysicalRect, PhysicalSize};

    #[test]
    fn centers_a4_paper_with_shadow_margin_in_offset_work_area() {
        let area = PhysicalRect {
            position: PhysicalPosition::new(-1920, 40),
            size: PhysicalSize::new(1920, 1040),
        };
        let rect = board_editor_rect(area);
        assert_eq!(rect.size, PhysicalSize::new(703, 971));
        assert_eq!(rect.position, PhysicalPosition::new(-1312, 74));
        assert_eq!(rect.size.height - 56, 915);
        assert_eq!(rect.size.width - 56, 647);
    }

    #[test]
    fn clamps_small_work_area_width_and_keeps_window_inside_it() {
        let area = PhysicalRect {
            position: PhysicalPosition::new(2200, -300),
            size: PhysicalSize::new(640, 900),
        };
        let rect = board_editor_rect(area);
        assert!(rect.size.width <= 614);
        assert!(rect.size.height <= area.size.height);
        assert_eq!(rect.position.x, area.position.x + (640 - rect.size.width as i32) / 2);
        assert_eq!(rect.position.y, area.position.y + (900 - rect.size.height as i32) / 2);
        assert!((f64::from(rect.size.width - 56) / f64::from(rect.size.height - 56)
            - 210.0 / 297.0).abs() < 0.002);
    }

    #[test]
    fn resized_paper_keeps_a4_ratio_and_clamps_to_offset_work_area() {
        let area = PhysicalRect { position: PhysicalPosition::new(-1920, 40), size: PhysicalSize::new(1200, 900) };
        let current = PhysicalRect { position: PhysicalPosition::new(-900, 700), size: PhysicalSize::new(600, 800) };
        let rect = resized_board_editor_rect(area, current, 800);
        assert!(rect.position.x >= area.position.x);
        assert!(rect.position.y >= area.position.y);
        assert!(rect.position.x + rect.size.width as i32 <= area.position.x + area.size.width as i32);
        assert!(rect.position.y + rect.size.height as i32 <= area.position.y + area.size.height as i32);
        assert!((f64::from(rect.size.width - 56) / f64::from(rect.size.height - 56) - 210.0 / 297.0).abs() < 0.002);
    }
}

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
#[cfg_attr(not(windows), allow(unused_variables))] // app 仅 Win32 sink 路径使用
fn shell_sink(app: tauri::AppHandle) -> Result<(), CmdError> {
    #[cfg(windows)]
    if let Some(h) = sticky_hwnd(&app) {
        shell::sink(h);
    }
    #[cfg(not(windows))]
    {
        let w = app.get_webview_window("sticky")
            .ok_or_else(|| CmdError::internal("sticky window does not exist"))?;
        #[cfg(target_os = "macos")]
        position::save(&w.as_ref().window())
            .map_err(|e| CmdError::internal(format!("save sticky position failed: {e}")))?;
        w.hide()
            .map_err(|e| CmdError::internal(format!("hide sticky window failed: {e}")))?;
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

// async command：WebviewWindowBuilder::build() 在 Windows 上需回主事件循环初始化
// WebView；sync command 线程阻塞等待会与主线程互等死锁（invoke 永久 pending）。
#[tauri::command]
async fn set_board_window(zoomed: bool, app: tauri::AppHandle) -> Result<(), CmdError> {
    if zoomed {
        let sticky = app
            .get_webview_window("sticky")
            .ok_or_else(|| CmdError::internal("sticky window does not exist"))?;
        let monitor = sticky.current_monitor()
            .map_err(|e| CmdError::internal(format!("read monitor failed: {e}")))?
            .ok_or_else(|| CmdError::internal("current monitor does not exist"))?;
        let rect = board_editor_rect(*monitor.work_area());
        let editor = if let Some(editor) = app.get_webview_window("board-editor") {
            editor
        } else {
            // 同进程第二个 WebView2 Environment 若与首窗的 browser arguments 不一致,
            // 控制器创建报 0x8007139F ERROR_INVALID_STATE——必须复用 sticky 的参数。
            let browser_args = app.config().app.windows.iter()
                .find(|w| w.label == "sticky")
                .and_then(|w| w.additional_browser_args.clone())
                .unwrap_or_default();
            tauri::WebviewWindowBuilder::new(&app, "board-editor",
                    tauri::WebviewUrl::App("index.html#/board-editor".into()))
                .title("日課·便签")
                .additional_browser_args(&browser_args)
                .inner_size(f64::from(rect.size.width) / monitor.scale_factor(),
                            f64::from(rect.size.height) / monitor.scale_factor())
                .position(f64::from(rect.position.x) / monitor.scale_factor(),
                          f64::from(rect.position.y) / monitor.scale_factor())
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .resizable(false)
                .skip_taskbar(true)
                .visible(true)
                .build()
                .map_err(|e| CmdError::internal(format!("create board-editor window failed: {e}")))?
        };
        editor.set_size(rect.size)
            .map_err(|e| CmdError::internal(format!("resize board-editor window failed: {e}")))?;
        editor.set_position(rect.position)
            .map_err(|e| CmdError::internal(format!("center board window failed: {e}")))?;
        editor.show()
            .map_err(|e| CmdError::internal(format!("show board-editor window failed: {e}")))?;
        BOARD_WINDOW_ZOOMED.store(true, Ordering::SeqCst);
        editor.set_focus()
            .map_err(|e| CmdError::internal(format!("focus board-editor window failed: {e}")))?;
    } else {
        if let Some(editor) = app.get_webview_window("board-editor") {
            editor.hide()
                .map_err(|e| CmdError::internal(format!("hide board-editor window failed: {e}")))?;
        }
        BOARD_WINDOW_ZOOMED.store(false, Ordering::SeqCst);
    }
    Ok(())
}

fn sticky_day(date: Option<&str>) -> Result<chrono::NaiveDate, CmdError> {
    let repo::FileKind::Day(day) = store::resolve_kind("day", date)? else {
        return Err(CmdError::bad_request("应为日期"));
    };
    Ok(day)
}

#[tauri::command]
fn list_stickies(notebook_id: String, state: tauri::State<StoreState>) -> Result<store::StickiesDto, CmdError> {
    state.list_stickies(&notebook_id)
}

#[tauri::command]
fn add_sticky(notebook_id: String, content: String, state: tauri::State<StoreState>) -> Result<store::StickyCreatedDto, CmdError> {
    state.add_sticky(&notebook_id, &content)
}

#[tauri::command]
fn save_sticky(notebook_id: String, date: Option<String>, id: String, content: String, base_version: String, state: tauri::State<StoreState>) -> Result<store::EditResult, CmdError> {
    state.save_sticky(&notebook_id, sticky_day(date.as_deref())?, &id, &content, parse_bv(&base_version)?)
}

#[tauri::command]
fn delete_sticky(notebook_id: String, date: Option<String>, id: String, base_version: String, state: tauri::State<StoreState>) -> Result<store::StickyDeletedDto, CmdError> {
    state.delete_sticky(&notebook_id, sticky_day(date.as_deref())?, &id, parse_bv(&base_version)?)
}

#[tauri::command]
fn restore_sticky(notebook_id: String, date: Option<String>, id: String, content: String, state: tauri::State<StoreState>) -> Result<store::EditResult, CmdError> {
    state.restore_sticky(&notebook_id, sticky_day(date.as_deref())?, &id, &content)
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
fn settle_pomo(
    notebook_id: String,
    kind: String,
    date: Option<String>,
    line_idx: usize,
    base_version: String,
    expected_ts: u64,
    state: tauri::State<StoreState>,
) -> Result<store::EditResult, CmdError> {
    state.settle_pomo(&notebook_id, &view_kind(&kind, date.as_deref())?, line_idx, parse_bv(&base_version)?, expected_ts)
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

/// 本机提交身份（页脚右下角展示，如 sticky@<machine>）。
#[tauri::command]
fn machine_tag() -> String {
    crate::sync::machine_tag()
}

/// 窗口壳装配：位置恢复/默认右下角（placement）→ 沉回桌面层 → 注册 Alt+S 全局
/// 热键（设计方案 §0）。热键注册失败只记日志不崩（可能被其他程序占用）。
#[cfg_attr(not(windows), allow(unused_variables))] // win 仅 Win32 壳路径使用
fn setup_shell(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let Some(win) = app.get_webview_window("sticky") else {
        return Err("sticky 窗口未配置".into());
    };

    #[cfg(windows)]
    if let Ok(h) = win.hwnd() {
        placement::init(h.0); // 恢复上次位置（无效则默认工作区右下角贴齐）
        shell::sink_to_bottom(h.0);
        shell::spawn_fullscreen_watch(h.0); // F12：前台全屏自动隐藏
        edge_hide::spawn(h.0);
    }

    #[cfg(target_os = "macos")]
    position::init(&win.as_ref().window())?;

    // mac 开机自启（W4）：每次启动重写 LaunchAgent plist——应用更新/换目录后
    // 路径自愈；v1 无 UI 开关，手动移除登录项会在下次启动恢复。
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_autostart::ManagerExt;
        if let Err(e) = app.autolaunch().enable() {
            eprintln!("[shell] enable autostart failed: {e}");
        }
    }

    #[cfg(windows)]
    update::spawn(app.handle().clone());

    #[cfg(desktop)]
    register_hotkey(app);

    Ok(())
}

/// Alt+S 全局热键：桌面专属能力（移动端不注册插件，编译期排除）。
#[cfg(desktop)]
fn register_hotkey(app: &tauri::App) {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    let gs = app.global_shortcut();
    #[cfg(windows)]
    let shortcut = "alt+s";
    #[cfg(not(windows))]
    let shortcut = "Command+Alt+S";
    if let Err(e) = gs.on_shortcut(shortcut, |_app, _sc, event| {
        let pressed = event.state == ShortcutState::Pressed;
        #[cfg(windows)]
        if pressed && shell::is_sunk() {
            if let Some(w) = _app.get_webview_window("sticky") {
                if let Ok(h) = w.hwnd() {
                    shell::summon(h.0);
                }
                let _ = w.set_focus();
            }
        }
        #[cfg(not(windows))]
        if pressed {
            if let Some(w) = _app.get_webview_window("sticky") {
                if let Err(e) = w.show() {
                    eprintln!("[shell] show sticky window failed: {e}");
                } else if let Err(e) = w.set_focus() {
                    eprintln!("[shell] focus sticky window failed: {e}");
                }
            }
        }
    }) {
        eprintln!("[shell] Alt+S 热键注册失败（可能被占用）: {e}");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    {
        update::early_restarter();
        update::cleanup_old();
    }
    // tauri-runtime-wry 建窗/建 WebView 失败只走 log::error!，不装 logger 会静默吞错
    //（曾致 board-editor 窗口建不出来且无任何报错）——Error 级打到 stderr 以便排查。
    #[cfg(desktop)]
    {
        struct StderrLog;
        impl log::Log for StderrLog {
            fn enabled(&self, _: &log::Metadata) -> bool { true }
            // eprintln! 在 stderr 写失败（GUI 启动无有效句柄）时按 std 语义 panic，
            // 恰好炸在建窗失败现场——改 writeln! 并忽略错误。
            fn log(&self, r: &log::Record) {
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "[log:{}] {}", r.level(), r.args());
            }
            fn flush(&self) {}
        }
        static LOGGER: StderrLog = StderrLog;
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Error);
    }
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
            // 便签本
            list_stickies,
            add_sticky,
            save_sticky,
            delete_sticky,
            restore_sticky,
            get_yesterday_leftovers,
            copy_leftover_to_today,
            set_checked,
            set_category,
            set_priority,
            set_flag,
            set_status,
            settle_pomo,
            set_content,
            delete_task,
            restore_deleted,
            set_blocked,
            add_task,
            sync_now,
            sync_config::get_sync_config,
            sync_config::set_sync_config,
            #[cfg(windows)]
            update::update_status,
            #[cfg(windows)]
            update::update_check_now,
            sync_status,
            machine_tag,
            shell_sink,
            set_body_height,
            set_board_window,
            resize_board_editor
        ])
        // 拖动落定：Moved 只记账，placement 监视线程防抖后吸附+落盘
        .on_window_event(|window, event| {
            if window.label() == "board-editor" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    use tauri::Emitter;
                    api.prevent_close();
                    if let Err(error) = window.hide() {
                        log_err!("hide board-editor window failed: {error}");
                    } else {
                        BOARD_WINDOW_ZOOMED.store(false, Ordering::SeqCst);
                        if let Some(sticky) = window.app_handle().get_webview_window("sticky") {
                            if let Err(error) = sticky.emit("board-editor-dismissed", serde_json::json!({})) {
                                log_err!("emit board-editor-dismissed failed: {error}");
                            }
                        }
                    }
                }
            }
            // sticky 销毁即整体退出：board-editor 的关闭被拦截为 hide 永不销毁，
            // 只关 sticky 时「最后一窗关闭」的 ExitRequested(code:None) 不会触发，
            // 进程残留成僵尸——单实例回调拿不到 sticky，桌面快捷方式从此点不开
            //（2026-10-07 实测命中）。主窗口即应用生命周期：Destroyed 主动走
            // exit(0) → ExitRequested(code:Some(0)) 进入下方 flush 通道。
            if window.label() == "sticky" {
                if let tauri::WindowEvent::Destroyed = event {
                    window.app_handle().exit(0);
                }
            }
            if let tauri::WindowEvent::Moved(_) = event {
                if window.label() == "sticky" && !BOARD_WINDOW_ZOOMED.load(Ordering::SeqCst) {
                    #[cfg(windows)]
                    if let Ok(h) = window.hwnd() {
                        placement::note_moved(h.0);
                    }
                    #[cfg(target_os = "macos")]
                    position::note_moved(window);
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
            #[cfg(windows)]
            if let Ok(h) = win.hwnd() {
                edge_hide::ensure_shown(h.0);
            }
            let _ = win.set_focus();
        }
    }));

    // 全局热键插件为桌面专属（见 register_hotkey）；移动端不挂载。
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());

    // mac 开机自启（W4）：LaunchAgent 方式；依赖仅 mac 目标引入，Win 构建不挂载。
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        None,
    ));

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
            #[cfg(target_os = "macos")]
            if let Some(w) = app_handle.get_webview_window("sticky") {
                if let Err(e) = position::save(&w.as_ref().window()) {
                    eprintln!("[shell] save sticky position failed: {e}");
                }
            }
            if !flushed {
                flushed = true;
                // code=None（最后一窗关闭）须 prevent 才有机会先跑 flush 再退出；
                // code=Some(0)（sticky Destroyed 主动 exit(0)）已在退出途中，
                // 再 prevent 反而取消退出变真僵尸。
                if code.is_none() {
                    api.prevent_exit();
                }
                app_handle.state::<StoreState>().detach_scheduler();
                if let Some(s) = sched.take() {
                    s.stop(Duration::from_secs(15));
                }
                app_handle.exit(0);
            }
        }
    });
}
