//! 窗口摆放：报头拖动 + 边缘吸附 + 位置记忆（2026-09-11 用户需求）。
//!
//! 拖动本身由系统接管（报头是 data-tauri-drag-region）；本模块负责拖完落定：
//! - WindowEvent::Moved 防抖 200ms → 距所在显示器工作区边缘 ≤24 逻辑像素
//!   则吸附贴齐（角落=两边同时贴），位置持久化到
//!   `%APPDATA%\sticky-todo\window.json`（每机独立，不参与同步）。
//! - 吸附底边后内容增高向上生长（resize_anchored 锚底边，否则锚左上）。
//! - 启动恢复上次位置；保存位置的中心点已不在任何显示器上（换分辨率/拔
//!   屏幕）则回退默认右下角。
//!
//! 坐标全部物理像素（GetWindowRect / rcWork 本就物理；DPI 换算只用于阈值）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

/// 吸附判定阈值（逻辑像素；物理阈值 = 本值 × 窗口 DPI 缩放）。
const SNAP_LOGICAL: i32 = 24;
/// 落定防抖：最后一次 Moved 后静默多久算拖动结束。
const SETTLE_MS: i64 = 200;

static MOVED_AT: AtomicI64 = AtomicI64::new(0);
static DIRTY: AtomicBool = AtomicBool::new(false);
static WATCH_HWND: AtomicUsize = AtomicUsize::new(0);
/// 底边吸附中：内容增高时向上生长。
static ANCHOR_BOTTOM: AtomicBool = AtomicBool::new(false);
/// 上次落盘的位置（跳过重复写盘；也是 settle 收敛哨兵）。
static LAST_SAVED: Mutex<(i32, i32)> = Mutex::new((i32::MIN, i32::MIN));

// ============ 纯函数（单元测试覆盖） ============

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// 窗口四边哪些距工作区对应边 ≤ 阈值。
pub fn snap_edges(rect: &RECT, work: &RECT, thr: i32) -> Edges {
    Edges {
        left: rect.left - work.left <= thr,
        right: work.right - rect.right <= thr,
        top: rect.top - work.top <= thr,
        bottom: work.bottom - rect.bottom <= thr,
    }
}

/// 吸附后的左上角坐标（两边同时命中即贴角落；无命中保持原位）。
pub fn snapped_pos(rect: &RECT, work: &RECT, e: Edges) -> (i32, i32) {
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    let x = if e.left { work.left } else if e.right { work.right - w } else { rect.left };
    let y = if e.top { work.top } else if e.bottom { work.bottom - h } else { rect.top };
    (x, y)
}

/// 默认落点：工作区右下角贴齐。
pub fn default_pos(work: &RECT, w: i32, h: i32) -> (i32, i32) {
    (work.right - w, work.bottom - h)
}

/// 把左上角钳制进工作区（窗口比工作区大时贴左上，保证可见）。
pub fn clamp_into_work(x: i32, y: i32, w: i32, h: i32, work: &RECT) -> (i32, i32) {
    let max_x = (work.right - w).max(work.left);
    let max_y = (work.bottom - h).max(work.top);
    (x.clamp(work.left, max_x), y.clamp(work.top, max_y))
}

/// 保存的位置还有效吗：窗口中心点落在显示器矩形内（换分辨率/拔屏幕检测）。
pub fn center_on_monitor(x: i32, y: i32, w: i32, h: i32, mon: &RECT) -> bool {
    let cx = x + w / 2;
    let cy = y + h / 2;
    cx >= mon.left && cx < mon.right && cy >= mon.top && cy < mon.bottom
}

/// 恢复探测点必须来自保存位置；当前启动屏幕和默认高度不代表上次所在屏幕。
fn saved_monitor_point(saved: &SavedPos, w: i32) -> (i32, i32) {
    (saved.x + w / 2, saved.y)
}

fn restored_pos(saved: &SavedPos, w: i32, h: i32, work: &RECT, mon: &RECT) -> Option<(i32, i32)> {
    let (cx, cy) = saved_monitor_point(saved, w);
    if !center_on_monitor(cx, cy, 0, 0, mon) {
        return None;
    }
    let y = if saved.bottom { work.bottom - h } else { saved.y };
    Some(clamp_into_work(saved.x, y, w, h, work))
}

/// 高 DPI / 小屏幕下限制外框尺寸，让 WebView 的滚动区始终可达。
fn fitted_resize(rect: &RECT, w: i32, h: i32, work: &RECT, bottom: bool) -> Option<RECT> {
    let work_w = work.right - work.left;
    let work_h = work.bottom - work.top;
    if work_w <= 0 || work_h <= 0 {
        return None;
    }
    let w = w.clamp(1, work_w);
    let h = h.clamp(1, work_h);
    let y = if bottom { rect.bottom - h } else { rect.top };
    let (x, y) = clamp_into_work(rect.left, y, w, h, work);
    Some(RECT { left: x, top: y, right: x + w, bottom: y + h })
}

// ============ 持久化（%APPDATA%\sticky-todo\window.json） ============

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SavedPos {
    pub x: i32,
    pub y: i32,
    pub bottom: bool,
}

fn window_json() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("sticky-todo").join("window.json"))
}

fn save_pos(x: i32, y: i32, bottom: bool) {
    {
        let mut last = LAST_SAVED.lock().unwrap();
        if *last == (x, y) {
            return;
        }
        *last = (x, y);
    }
    let Some(path) = window_json() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let s = SavedPos { x, y, bottom };
    if let Ok(json) = serde_json::to_string(&s) {
        let _ = std::fs::write(path, json);
    }
}

fn load_pos() -> Option<SavedPos> {
    let path = window_json()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ============ Win32 落地 ============

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

unsafe fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    (GetWindowRect(hwnd, &mut r) != 0).then_some(r)
}

/// 窗口所在（最近）显示器的工作区。
unsafe fn work_of(hwnd: HWND) -> RECT {
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
        return RECT { left: 0, top: 0, right: 0, bottom: 0 };
    }
    mi.rcWork
}

/// 某点所在显示器的工作区 + 全屏矩形（找不到返回 None）。
unsafe fn monitor_at(cx: i32, cy: i32) -> Option<(RECT, RECT)> {
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let mon = MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST);
    if mon.is_null() || GetMonitorInfoW(mon, &mut mi) == 0 {
        return None;
    }
    Some((mi.rcWork, mi.rcMonitor))
}

/// 窗口 DPI 缩放（逻辑→物理）。
unsafe fn scale_of(hwnd: HWND) -> f64 {
    (GetDpiForWindow(hwnd).max(96) as f64) / 96.0
}

unsafe fn move_to(hwnd: HWND, x: i32, y: i32) {
    SetWindowPos(
        hwnd,
        std::ptr::null_mut(),
        x,
        y,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// 拖动落定：吸附 + 锚定标记 + 落盘。
unsafe fn settle(hwnd: HWND) {
    let Some(rect) = window_rect(hwnd) else { return };
    let work = work_of(hwnd);
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if !monitor.is_null()
        && GetMonitorInfoW(monitor, &mut mi) != 0
        && crate::edge_hide::is_hidden_signature(&rect, &work, &mi.rcMonitor)
    {
        return;
    }
    let thr = (SNAP_LOGICAL as f64 * scale_of(hwnd)).round() as i32;
    let e = snap_edges(&rect, &work, thr);
    let (x, y) = snapped_pos(&rect, &work, e);
    ANCHOR_BOTTOM.store(e.bottom, Ordering::SeqCst);
    eprintln!("[place] settle rect=({},{},{},{}) snap=({},{}) edges=l{}r{}t{}b{}",
        rect.left, rect.top, rect.right, rect.bottom, x, y, e.left, e.right, e.top, e.bottom);
    if x != rect.left || y != rect.top {
        // 吸附移动本身会再触发 Moved → 二次 settle 算出同坐标不再移动，收敛
        move_to(hwnd, x, y);
    }
    save_pos(x, y, e.bottom);
}

/// WindowEvent::Moved 回调：只记账，落定交给监视线程防抖。
pub fn note_moved(hwnd: HWND) {
    WATCH_HWND.store(hwnd as usize, Ordering::SeqCst);
    MOVED_AT.store(now_ms(), Ordering::SeqCst);
    DIRTY.store(true, Ordering::SeqCst);
}

fn spawn_settle_thread() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(60));
        if DIRTY.load(Ordering::SeqCst) && now_ms() - MOVED_AT.load(Ordering::SeqCst) >= SETTLE_MS
        {
            DIRTY.store(false, Ordering::SeqCst);
            let hwnd = WATCH_HWND.load(Ordering::SeqCst) as HWND;
            if !hwnd.is_null() {
                unsafe { settle(hwnd) };
            }
        }
    });
}

/// 启动装配：恢复上次位置（无效则默认右下角）+ 启动落定监视线程。
///
/// bottom=true 时的 y 只是保存时刻的快照：恢复时窗口还是配置默认高度
/// （内容量高发生在前端加载后），直接 move_to(y) 会让底边落在半空，
/// 随后 resize_anchored 锚到这个错误的过渡底边（E2E P3 实测悬空 332px）。
/// 所以 bottom 恢复一律按「工作区底边 − 当前高度」重算。
pub(crate) fn init(hwnd: HWND) {
    unsafe {
        let Some(rect) = window_rect(hwnd) else { return };
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        let placed = load_pos().and_then(|s| {
            let (cx, cy) = saved_monitor_point(&s, w);
            let (work, mon) = monitor_at(cx, cy)?;
            restored_pos(&s, w, h, &work, &mon).map(|pos| (pos, s.bottom))
        });
        let ((x, y), bottom) = placed.unwrap_or_else(|| {
            let work = work_of(hwnd);
            let (x, y) = default_pos(&work, w, h);
            (clamp_into_work(x, y, w, h, &work), true)
        });
        eprintln!("[place] init rect=({},{},{},{}) -> ({x},{y}) bottom={bottom}",
            rect.left, rect.top, rect.right, rect.bottom);
        move_to(hwnd, x, y);
        ANCHOR_BOTTOM.store(bottom, Ordering::SeqCst);
        save_pos(x, y, bottom);
    }
    spawn_settle_thread();
}

/// 内容高度自适应：宽固定；锚定底边时向上生长，否则保持左上。
/// h 已由调用方钳制（200–1600）。单次 SetWindowPos 同时给尺寸与位置，
/// 避免先长高再跳位的两帧闪烁。
///
/// 注意 SetWindowPos 给的是**含不可见 DWM 边框**的窗口尺寸（本机实测双侧
/// 合计 22 物理像素），而 w/h 传入的是客户区（WebView 内容）尺寸——必须
/// 用当前窗口-client 差值补上，否则客户区会被压窄 22px（探针
/// probe_winpos.py 实测：客户区 558→536）。边框由窗口样式决定，恒定。
pub(crate) fn resize_anchored(hwnd: HWND, w_logical: f64, h_logical: f64) {
    unsafe {
        let scale = scale_of(hwnd);
        let mut cr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetClientRect(hwnd, &mut cr) == 0 {
            return;
        }
        let Some(r) = window_rect(hwnd) else { return };
        let border_x = (r.right - r.left) - cr.right;
        let border_y = (r.bottom - r.top) - cr.bottom;
        let w = (w_logical * scale).round() as i32 + border_x;
        let h = (h_logical * scale).round() as i32 + border_y;
        let work = work_of(hwnd);
        let Some(fitted) = fitted_resize(&r, w, h, &work, ANCHOR_BOTTOM.load(Ordering::SeqCst)) else { return };
        let w = fitted.right - fitted.left;
        let h = fitted.bottom - fitted.top;
        let y = fitted.top;
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let x = if !monitor.is_null() && GetMonitorInfoW(monitor, &mut mi) != 0 {
            crate::edge_hide::hidden_x_override(w, &work, &mi.rcMonitor)
                .unwrap_or(fitted.left)
        } else {
            fitted.left
        };
        eprintln!("[place] resize w={w_logical} h={h_logical} -> outer {w}x{h} at y={y} anchor_bottom={}", ANCHOR_BOTTOM.load(Ordering::SeqCst));
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: r, bottom: b }
    }

    #[test]
    fn long_content_fits_work_area_and_shrinks_from_bottom() {
        let work = rect(0, 0, 1920, 1040);
        let original = rect(1340, 640, 1920, 1040);
        let tall = fitted_resize(&original, 580, 1372, &work, true).unwrap();
        assert_eq!((tall.top, tall.bottom), (0, 1040));
        let short = fitted_resize(&tall, 580, 400, &work, true).unwrap();
        assert_eq!((short.top, short.bottom), (640, 1040));
    }

    #[test]
    fn upper_monitor_restore_uses_saved_position() {
        let saved = SavedPos { x: 1340, y: -500, bottom: true };
        let upper = rect(0, -1080, 1920, 0);
        let work = rect(0, -1080, 1920, -40);
        assert_eq!(saved_monitor_point(&saved, 580), (1630, -500));
        assert_eq!(restored_pos(&saved, 580, 700, &work, &upper), Some((1340, -740)));
        assert!(restored_pos(&saved, 580, 700, &rect(0, 0, 1920, 1040), &rect(0, 0, 1920, 1080)).is_none());
    }

    #[test]
    fn narrow_or_invalid_work_area_is_handled() {
        let window = rect(900, 900, 1480, 1300);
        let fitted = fitted_resize(&window, 580, 400, &rect(0, 0, 500, 300), false).unwrap();
        assert_eq!((fitted.left, fitted.top, fitted.right, fitted.bottom), (0, 0, 500, 300));
        assert!(fitted_resize(&window, 580, 400, &rect(0, 0, 0, 0), false).is_none());
    }

    #[test]
    fn edges_detect_threshold() {
        let work = rect(0, 0, 2560, 1400);
        // 右下角内 6px：right+bottom 命中，left/top 远
        let e = snap_edges(&rect(2100, 1300, 2554, 1394), &work, 24);
        assert_eq!(e, Edges { left: false, right: true, top: false, bottom: true });
        // 正好在阈值上：命中
        let e = snap_edges(&rect(24, 0, 400, 200), &work, 24);
        assert!(e.left && e.top && !e.right && !e.bottom);
        // 阈值 +1：不命中
        let e = snap_edges(&rect(25, 25, 400, 200), &work, 24);
        assert!(!e.left && !e.top && !e.right && !e.bottom);
    }

    #[test]
    fn snap_positions_flush() {
        let work = rect(0, 0, 2560, 1400);
        // 右下角 → 贴齐右下
        let r = rect(2100, 1300, 2554, 1394);
        let (x, y) = snapped_pos(&r, &work, snap_edges(&r, &work, 24));
        assert_eq!((x, y), (2560 - 454, 1400 - 94));
        // 只贴右边 → x 贴齐、y 保持
        let r = rect(2200, 600, 2558, 900);
        let (x, y) = snapped_pos(&r, &work, snap_edges(&r, &work, 24));
        assert_eq!((x, y), (2560 - 358, 600));
        // 无命中 → 原位
        let r = rect(800, 500, 1200, 800);
        let (x, y) = snapped_pos(&r, &work, snap_edges(&r, &work, 24));
        assert_eq!((x, y), (800, 500));
    }

    #[test]
    fn default_is_bottom_right() {
        assert_eq!(default_pos(&rect(0, 0, 2560, 1400), 558, 840), (2560 - 558, 1400 - 840));
    }

    #[test]
    fn clamp_keeps_visible() {
        let work = rect(0, 0, 2560, 1400);
        // 完全在工作区外 → 钳回
        assert_eq!(clamp_into_work(-5000, -5000, 558, 840, &work), (0, 0));
        assert_eq!(clamp_into_work(9000, 2000, 558, 840, &work), (2560 - 558, 1400 - 840));
        // 窗口比工作区高 → 贴左上，不倒挂
        assert_eq!(clamp_into_work(100, 100, 558, 2000, &work), (100, 0));
        // 在界内不动
        assert_eq!(clamp_into_work(300, 300, 558, 840, &work), (300, 300));
    }

    #[test]
    fn saved_validity_by_center() {
        let mon = rect(-1440, 0, 2400, 2160);
        assert!(center_on_monitor(100, 100, 558, 840, &mon));
        assert!(!center_on_monitor(-4000, 100, 558, 840, &mon)); // 中心在显示器左界外
        assert!(!center_on_monitor(2400 - 558 / 2, 500, 558, 840, &mon)); // 中心恰在右界=界外
    }

    #[test]
    fn saved_pos_json_roundtrip() {
        let s = SavedPos { x: 123, y: -45, bottom: true };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<SavedPos>(&json).unwrap(), s);
        assert!(serde_json::from_str::<SavedPos>("垃圾").is_err());
    }

    #[test]
    fn resize_keeps_unanchored_bottom_visible_and_small_content_in_place() {
        let work = rect(-1920, -1080, 0, -60);
        let old = rect(-600, -500, -20, -200);
        let fitted = fitted_resize(&old, 580, 900, &work, false).unwrap();
        assert_eq!((fitted.top, fitted.bottom), (-960, -60));
        let unchanged = fitted_resize(&old, 580, 300, &work, false).unwrap();
        assert_eq!((unchanged.left, unchanged.top), (-600, -500));
        assert!(fitted_resize(&old, 580, 900, &rect(0, 0, 0, 0), true).is_none());
    }

    #[test]
    fn ordinary_partly_offscreen_drag_is_not_hidden_signature() {
        let work = rect(0, 0, 1920, 1040);
        let mon = rect(0, 0, 1920, 1080);
        let hidden = rect(-576, 200, 4, 700);
        let dragged = rect(-80, 200, 500, 700);
        assert!(matches!(
            crate::edge_hide::classify(&hidden, &work, &mon, 2, 4, true),
            crate::edge_hide::Where::DockedHidden(_)
        ));
        assert!(!matches!(
            crate::edge_hide::classify(&dragged, &work, &mon, 2, 4, true),
            crate::edge_hide::Where::DockedHidden(_)
        ));
        assert!(snap_edges(&dragged, &work, 24).left);
    }

}
