//! 主面板左右吸边自动隐藏（仅 Windows）。
//!
//! 窗口只有贴齐显示器工作区的可达侧边时才会隐藏；侧边任务栏会挡住触发条，
//! 因而该侧明确禁用。隐藏只把窗口横向移出屏幕并留下细边，不改变可见性和 Z 序，
//! 这样全屏看门狗仍独占 ShowWindow 职责，沉底/Alt+S 也仍独占 Z 序职责。
//! 每次轮询都重新读取显示器几何，以便 DPI、拓扑和跨屏变化自行收敛。程序主动
//! placement 落定通过隐藏签名跳过屏外位置，显示位移动则正常持久化。

use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

const SLIVER_LOGICAL: i32 = 4;
const TRIGGER_LOGICAL: i32 = 8;
const POLL_MS: u64 = 50;
const REHIDE_MS: i64 = 350;
const FLUSH_TOL_PX: i32 = 2;

/// 0=悬浮，1=贴边显示，2=贴边隐藏。
static PHASE: AtomicU8 = AtomicU8::new(0);
/// 0=左，1=右。
static EDGE: AtomicU8 = AtomicU8::new(0);
/// 0=未武装，否则为隐藏截止时间。
static HIDE_AT: AtomicI64 = AtomicI64::new(0);
/// 当前窗口 DPI 下的物理细边宽度，供无 HWND 的 resize 辅助路径复用。
static SLIVER_PX: AtomicI32 = AtomicI32::new(SLIVER_LOGICAL);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left = 0,
    Right = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where {
    Floating,
    DockedShown(Side),
    DockedHidden(Side),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    None,
    ArmHide(i64),
    Hide,
    Reveal,
}

pub fn side_usable(work: &RECT, mon: &RECT, side: Side) -> bool {
    match side {
        Side::Left => work.left == mon.left,
        Side::Right => work.right == mon.right,
    }
}

pub fn probe_point(mon: &RECT, side: Side) -> POINT {
    let y = mon.top + (mon.bottom - mon.top) / 2;
    match side {
        Side::Left => POINT { x: mon.left - 1, y },
        Side::Right => POINT { x: mon.right, y },
    }
}

unsafe fn edge_is_outermost(mon: &RECT, side: Side) -> bool {
    let monitor = MonitorFromPoint(probe_point(mon, side), MONITOR_DEFAULTTONEAREST);
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if monitor.is_null() || GetMonitorInfoW(monitor, &mut mi) == 0 {
        return false;
    }
    mi.rcMonitor.left == mon.left
        && mi.rcMonitor.top == mon.top
        && mi.rcMonitor.right == mon.right
        && mi.rcMonitor.bottom == mon.bottom
}

/// 先识别隐藏签名；否则负向移出的窗口也会被“左边距 ≤ 容差”误认成显示贴齐。
pub fn classify(rect: &RECT, work: &RECT, mon: &RECT, tol: i32, sliver: i32, outermost: bool) -> Where {
    if !outermost {
        return Where::Floating;
    }
    let left_visible = rect.right - work.left;
    if side_usable(work, mon, Side::Left) && (0..=sliver + tol).contains(&left_visible) {
        return Where::DockedHidden(Side::Left);
    }
    let right_visible = work.right - rect.left;
    if side_usable(work, mon, Side::Right) && (0..=sliver + tol).contains(&right_visible) {
        return Where::DockedHidden(Side::Right);
    }
    if side_usable(work, mon, Side::Left) && (rect.left - work.left).abs() <= tol {
        return Where::DockedShown(Side::Left);
    }
    if side_usable(work, mon, Side::Right) && (rect.right - work.right).abs() <= tol {
        return Where::DockedShown(Side::Right);
    }
    Where::Floating
}

pub fn cursor_in(r: &RECT, p: &POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

pub fn trigger_rect(work: &RECT, side: Side, w_px: i32) -> RECT {
    match side {
        Side::Left => RECT { left: work.left, top: work.top, right: work.left + w_px, bottom: work.bottom },
        Side::Right => RECT { left: work.right - w_px, top: work.top, right: work.right, bottom: work.bottom },
    }
}

pub fn hidden_x(work: &RECT, side: Side, w: i32, sliver: i32) -> i32 {
    match side {
        Side::Left => work.left - w + sliver,
        Side::Right => work.right - sliver,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn tick(
    where_: Where,
    cursor: POINT,
    rect: &RECT,
    trigger: &RECT,
    drag: bool,
    zoomed: bool,
    fs_hidden: bool,
    sunk: bool,
    now_ms: i64,
    hide_at: i64,
) -> Tick {
    if drag || zoomed || fs_hidden {
        return Tick::None;
    }
    match where_ {
        Where::Floating => Tick::None,
        Where::DockedShown(_) if !sunk => Tick::None,
        Where::DockedShown(_) => {
            if cursor_in(rect, &cursor) || cursor_in(trigger, &cursor) {
                Tick::None
            } else if hide_at == 0 {
                Tick::ArmHide(now_ms + REHIDE_MS)
            } else if now_ms >= hide_at {
                Tick::Hide
            } else {
                Tick::None
            }
        }
        Where::DockedHidden(_) if cursor_in(trigger, &cursor) || !sunk => Tick::Reveal,
        Where::DockedHidden(_) => Tick::None,
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

unsafe fn geometry(hwnd: HWND) -> Option<(RECT, RECT, RECT)> {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if GetWindowRect(hwnd, &mut rect) == 0 {
        return None;
    }
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.is_null() || GetMonitorInfoW(monitor, &mut mi) == 0 {
        return None;
    }
    Some((rect, mi.rcWork, mi.rcMonitor))
}

unsafe fn scale_of(hwnd: HWND) -> f64 {
    f64::from(GetDpiForWindow(hwnd).max(96)) / 96.0
}

fn set_state(where_: Where) {
    match where_ {
        Where::Floating => PHASE.store(0, Ordering::SeqCst),
        Where::DockedShown(side) => {
            EDGE.store(side as u8, Ordering::SeqCst);
            PHASE.store(1, Ordering::SeqCst);
        }
        Where::DockedHidden(side) => {
            EDGE.store(side as u8, Ordering::SeqCst);
            PHASE.store(2, Ordering::SeqCst);
        }
    }
}

unsafe fn poll(hwnd: HWND) {
    let Some((rect, work, mon)) = geometry(hwnd) else { return };
    let sliver = (f64::from(SLIVER_LOGICAL) * scale_of(hwnd)).round() as i32;
    SLIVER_PX.store(sliver, Ordering::SeqCst);
    let trigger_w = (f64::from(TRIGGER_LOGICAL) * scale_of(hwnd)).round() as i32;
    let candidate = classify(&rect, &work, &mon, FLUSH_TOL_PX, sliver, true);
    let outermost = match candidate {
        Where::DockedShown(side) | Where::DockedHidden(side) => edge_is_outermost(&mon, side),
        Where::Floating => true,
    };
    let where_ = classify(&rect, &work, &mon, FLUSH_TOL_PX, sliver, outermost);
    set_state(where_);
    let side = match where_ {
        Where::DockedShown(side) | Where::DockedHidden(side) => side,
        Where::Floating => Side::Left,
    };
    let trigger = trigger_rect(&work, side, trigger_w);
    let mut cursor = POINT { x: 0, y: 0 };
    if GetCursorPos(&mut cursor) == 0 {
        return;
    }
    let drag = GetAsyncKeyState(VK_LBUTTON as i32) < 0;
    let zoomed = crate::board_window_zoomed();
    let fs_hidden = crate::shell::is_hidden_by_fs();
    let sunk = crate::shell::is_sunk();
    let hide_at = HIDE_AT.load(Ordering::SeqCst);
    let action = tick(where_, cursor, &rect, &trigger, drag, zoomed, fs_hidden, sunk, now_ms(), hide_at);

    if drag || zoomed || fs_hidden || !sunk || matches!(where_, Where::Floating | Where::DockedHidden(_))
        || matches!(where_, Where::DockedShown(_)) && (cursor_in(&rect, &cursor) || cursor_in(&trigger, &cursor))
    {
        HIDE_AT.store(0, Ordering::SeqCst);
    }
    match action {
        Tick::None => {}
        Tick::ArmHide(at) => HIDE_AT.store(at, Ordering::SeqCst),
        Tick::Hide => {
            HIDE_AT.store(0, Ordering::SeqCst);
            slide(hwnd, hidden_x(&work, side, rect.right - rect.left, sliver));
            PHASE.store(2, Ordering::SeqCst);
            eprintln!("[edge] hide side={side:?}");
        }
        Tick::Reveal => {
            let width = rect.right - rect.left;
            let x = match side { Side::Left => work.left, Side::Right => work.right - width };
            slide(hwnd, x);
            PHASE.store(1, Ordering::SeqCst);
            eprintln!("[edge] reveal side={side:?}");
        }
    }
}

fn slide(hwnd: HWND, x: i32) {
    unsafe {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(hwnd, &mut rect) != 0 {
            SetWindowPos(hwnd, std::ptr::null_mut(), x, rect.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

pub(crate) fn spawn(hwnd: HWND) {
    if std::env::var_os("STICKY_EDGE_HIDE").is_some_and(|value| value == "0") {
        eprintln!("[edge] disabled by STICKY_EDGE_HIDE=0");
        return;
    }
    let raw = hwnd as usize;
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(POLL_MS));
        unsafe { poll(raw as HWND) };
    });
}

pub(crate) fn ensure_shown(hwnd: HWND) {
    if PHASE.load(Ordering::SeqCst) != 2 {
        return;
    }
    unsafe {
        let Some((rect, work, _)) = geometry(hwnd) else { return };
        let width = rect.right - rect.left;
        let side = if EDGE.load(Ordering::SeqCst) == 0 { Side::Left } else { Side::Right };
        slide(hwnd, match side { Side::Left => work.left, Side::Right => work.right - width });
        PHASE.store(1, Ordering::SeqCst);
        HIDE_AT.store(0, Ordering::SeqCst);
    }
}

pub(crate) fn is_hidden_signature(rect: &RECT, work: &RECT, mon: &RECT) -> bool {
    let candidate = classify(rect, work, mon, FLUSH_TOL_PX, SLIVER_PX.load(Ordering::SeqCst), true);
    let outermost = match candidate {
        Where::DockedHidden(side) => unsafe { edge_is_outermost(mon, side) },
        _ => false,
    };
    matches!(
        classify(rect, work, mon, FLUSH_TOL_PX, SLIVER_PX.load(Ordering::SeqCst), outermost),
        Where::DockedHidden(_)
    )
}

pub(crate) fn hidden_x_override(w: i32, work: &RECT, mon: &RECT) -> Option<i32> {
    if PHASE.load(Ordering::SeqCst) != 2 {
        return None;
    }
    let side = if EDGE.load(Ordering::SeqCst) == 0 { Side::Left } else { Side::Right };
    if !side_usable(work, mon, side) {
        return None;
    }
    // resize 路径没有 HWND；保留当前隐藏细边的物理宽度由调用前状态表示。
    Some(hidden_x(work, side, w, SLIVER_PX.load(Ordering::SeqCst)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rr: i32, b: i32) -> RECT { RECT { left: l, top: t, right: rr, bottom: b } }
    fn p(x: i32, y: i32) -> POINT { POINT { x, y } }

    #[test]
    fn classify_both_sides_hidden_negative_and_corner() {
        let work = r(-1920, 0, 0, 1040);
        let mon = r(-1920, 0, 0, 1080);
        assert_eq!(classify(&r(-1920, 100, -1500, 500), &work, &mon, 2, 4, true), Where::DockedShown(Side::Left));
        assert_eq!(classify(&r(-580, 100, 0, 500), &work, &mon, 2, 4, true), Where::DockedShown(Side::Right));
        assert_eq!(classify(&r(-2496, 100, -1916, 500), &work, &mon, 2, 4, true), Where::DockedHidden(Side::Left));
        assert_eq!(classify(&r(-4, 100, 576, 500), &work, &mon, 2, 4, true), Where::DockedHidden(Side::Right));
        assert_eq!(classify(&r(-1920, 640, -1340, 1040), &work, &mon, 2, 4, true), Where::DockedShown(Side::Left));
    }

    #[test]
    fn classify_rejects_grazing_and_taskbar_side() {
        let mon = r(0, 0, 1920, 1080);
        assert_eq!(classify(&r(3, 10, 583, 400), &r(0, 0, 1920, 1040), &mon, 2, 4, true), Where::Floating);
        assert_eq!(classify(&r(40, 10, 620, 400), &r(40, 0, 1920, 1080), &mon, 2, 4, true), Where::Floating);
        assert_eq!(classify(&r(1340, 10, 1880, 400), &r(0, 0, 1880, 1080), &mon, 2, 4, true), Where::Floating);
    }

    #[test]
    fn probe_points_are_just_outside_each_horizontal_edge() {
        let mon = r(-1920, -200, 0, 880);
        let left = probe_point(&mon, Side::Left);
        let right = probe_point(&mon, Side::Right);
        assert_eq!((left.x, left.y), (-1921, 340));
        assert_eq!((right.x, right.y), (0, 340));
    }

    #[test]
    fn classify_rejects_non_outermost_edges() {
        let work = r(0, 0, 1920, 1040);
        let mon = r(0, 0, 1920, 1080);
        assert_eq!(classify(&r(0, 100, 580, 500), &work, &mon, 2, 4, false), Where::Floating);
        assert_eq!(classify(&r(1340, 100, 1920, 500), &work, &mon, 2, 4, false), Where::Floating);
    }

    #[test]
    fn trigger_and_hidden_geometry() {
        let work = r(-1920, -200, 0, 880);
        let left = trigger_rect(&work, Side::Left, 12);
        assert_eq!((left.left, left.top, left.right, left.bottom), (-1920, -200, -1908, 880));
        let right = trigger_rect(&work, Side::Right, 12);
        assert_eq!((right.left, right.top, right.right, right.bottom), (-12, -200, 0, 880));
        assert_eq!(hidden_x(&work, Side::Left, 600, 6), -2514);
        assert_eq!(hidden_x(&work, Side::Right, 600, 6), -6);
    }

    #[test]
    fn tick_covers_transitions_and_suspensions() {
        let rect = r(0, 100, 400, 500);
        let trigger = r(0, 0, 8, 1040);
        let shown = Where::DockedShown(Side::Left);
        let hidden = Where::DockedHidden(Side::Left);
        assert_eq!(tick(Where::Floating, p(900, 900), &rect, &trigger, false, false, false, true, 10, 0), Tick::None);
        assert_eq!(tick(shown, p(900, 900), &rect, &trigger, false, false, false, true, 10, 0), Tick::ArmHide(360));
        assert_eq!(tick(shown, p(900, 900), &rect, &trigger, false, false, false, false, 10, 0), Tick::None);
        assert_eq!(tick(shown, p(900, 900), &rect, &trigger, false, false, false, true, 360, 360), Tick::Hide);
        assert_eq!(tick(shown, p(20, 200), &rect, &trigger, false, false, false, true, 500, 360), Tick::None);
        assert_eq!(tick(hidden, p(2, 200), &rect, &trigger, false, false, false, true, 1, 0), Tick::Reveal);
        assert_eq!(tick(hidden, p(900, 900), &rect, &trigger, false, false, false, false, 1, 0), Tick::Reveal);
        for (drag, zoomed, fs_hidden) in [(true, false, false), (false, true, false), (false, false, true)] {
            assert_eq!(tick(hidden, p(2, 200), &rect, &trigger, drag, zoomed, fs_hidden, false, 1, 0), Tick::None);
        }
    }

    #[test]
    fn cursor_bounds_are_half_open() {
        let rect = r(0, 0, 8, 10);
        assert!(cursor_in(&rect, &p(0, 0)));
        assert!(!cursor_in(&rect, &p(8, 9)));
    }
}
