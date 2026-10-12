//! 窗口壳：Z 序贴底常驻 + 热键唤起/沉回（设计方案 §0 层级模型的实现修正）。
//!
//! 原方案（SetParent 到壁纸 WorkerW）实测不可行：0x052C 布局从底到顶是
//! WorkerW(壁纸) → Progman(SHELLDLL_DefView→SysListView32 图标层) → 普通窗口，
//! 嵌入后贴纸在【图标层之下】，全屏的 SysListView32 接走所有点击，无法交互
//! （2026-09-10 本机实测：贴纸矩形内 4 个点 WindowFromPoint 全命中图标层）。
//!
//! 改用 Rainmeter "On Desktop" 式 Z 序管理：贴纸是普通窗口，
//! - 沉回（常态）：压到 HWND_BOTTOM —— 在所有普通窗口之下（开窗即被盖住）、
//!   仍在桌面图标层之上（可点击、可勾选）；
//! - 唤起（Alt+S）：HWND_TOPMOST + 前台焦点，5s 无操作或 Esc 沉回。
//!
//! 用户可见行为与原设计一致：桌面干净时可见、右上角常驻、Alt+S 唤起。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static SUNK: AtomicBool = AtomicBool::new(true);
static FOUND_PROGMAN: AtomicIsize = AtomicIsize::new(0);
/// 因全屏而隐藏（区别于用户沉回/最小化；只有隐藏者才负责恢复）
static HIDDEN_BY_FS: AtomicBool = AtomicBool::new(false);

const NULL_HWND: HWND = std::ptr::null_mut();

unsafe fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe extern "system" fn enum_find_progman(hwnd: HWND, _l: LPARAM) -> BOOL {
    if class_of(hwnd) == "Progman" {
        FOUND_PROGMAN.store(hwnd as isize, Ordering::SeqCst);
        return 0; // FALSE 停止枚举
    }
    1
}

/// 找 Progman。本机 FindWindowW("Progman") 稳定返回 0（系统怪象，
/// EnumWindows 却能看到），故用枚举实现。
unsafe fn find_progman() -> HWND {
    FOUND_PROGMAN.store(0, Ordering::SeqCst);
    EnumWindows(Some(enum_find_progman), 0);
    FOUND_PROGMAN.load(Ordering::SeqCst) as HWND
}

/// 取"桌面层之上"的 Z 序锚点：Progman 正上方的窗口。
/// HWND_BOTTOM 不能用：0x052C（壁纸引擎手法）残留创建的壁纸 WorkerW
/// 存在时，绝对底部会沉到画壁纸的 WorkerW 之下，被壁纸像素盖住。
unsafe fn above_desktop_anchor() -> HWND {
    let progman = find_progman();
    if progman.is_null() {
        return NULL_HWND;
    }
    GetWindow(progman, GW_HWNDPREV)
}

/// 启动时沉到桌面层之上（常态）。始终成功，无降级路径。
pub fn sink_to_bottom(hwnd: HWND) {
    sink(hwnd);
}

/// 唤起：置顶 + 前台 + 焦点（Alt+S）。
pub(crate) fn summon(hwnd: HWND) {
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(hwnd);
        SetFocus(hwnd);
    }
    // 用户主动唤起优先于全屏隐藏（隐藏循环的恢复职责随之解除）
    HIDDEN_BY_FS.store(false, Ordering::SeqCst);
    SUNK.store(false, Ordering::SeqCst);
}

/// 沉回：先脱离置顶带，再插到桌面层（Progman）正上方（5s 无操作 / Esc）。
/// 不激活、不动位置。
pub(crate) fn sink(hwnd: HWND) {
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let anchor = above_desktop_anchor();
        let target = if anchor.is_null() { HWND_BOTTOM } else { anchor };
        SetWindowPos(
            hwnd,
            target,
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    SUNK.store(true, Ordering::SeqCst);
}

pub fn is_sunk() -> bool {
    SUNK.load(Ordering::SeqCst)
}

/// 是否正由全屏看门狗隐藏；边缘隐藏据此暂停，避免和 ShowWindow 恢复抢状态。
pub(crate) fn is_hidden_by_fs() -> bool {
    HIDDEN_BY_FS.load(Ordering::SeqCst)
}

/// F12 全屏自动隐藏（PRD）：前台应用整盖贴纸所在显示器（视频/游戏/演示）
/// → 隐藏；退出全屏 ≤2s 恢复（1s 轮询）。只恢复自己隐藏的，不动唤起/沉回状态。
pub fn spawn_fullscreen_watch(hwnd: HWND) {
    // HWND（*mut c_void）非 Send；跨线程以 usize 携带
    let raw = hwnd as usize;
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        unsafe { poll_fullscreen(raw as HWND) };
    });
}

unsafe fn poll_fullscreen(hwnd: HWND) {
    // Alt+S explicitly summons the widget; honor it until the idle timer sinks it.
    if !is_sunk() {
        return;
    }
    let restore = |show: bool| {
        if show && HIDDEN_BY_FS.swap(false, Ordering::SeqCst) {
            ShowWindow(hwnd, SW_SHOW);
        }
    };
    let fg = GetForegroundWindow();
    if fg.is_null() || fg == hwnd {
        restore(true);
        return;
    }
    // 桌面/壁纸层不算全屏应用
    let cls = class_of(fg);
    if cls == "Progman" || cls == "WorkerW" {
        restore(true);
        return;
    }
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
        return;
    }
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if GetWindowRect(fg, &mut r) == 0 {
        return;
    }
    // 整盖显示器 = 全屏（最大化普通窗口只盖工作区，不含任务栏，不会误判）
    let covers = r.left <= mi.rcMonitor.left
        && r.top <= mi.rcMonitor.top
        && r.right >= mi.rcMonitor.right
        && r.bottom >= mi.rcMonitor.bottom;
    if covers {
        if IsWindowVisible(hwnd) != 0 {
            ShowWindow(hwnd, SW_HIDE);
            HIDDEN_BY_FS.store(true, Ordering::SeqCst);
        }
    } else {
        restore(true);
    }
}
