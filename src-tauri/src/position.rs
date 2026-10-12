//! macOS 贴纸窗口位置记忆（W3）：`~/Library/Application Support/sticky-todo/
//! window.json`。hide 前 / Moved 节流 / 退出三钩子落盘；Windows 走 placement.rs
//! （%APPDATA%），本模块仅 mac 编译。

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// 拖动时 Moved 事件高频触发：1s 内只落盘一次，hide/退出钩子兜住最终位置。
static LAST_SAVE_MS: AtomicI64 = AtomicI64::new(0);

#[derive(Deserialize, Serialize)]
struct SavedPosition {
    x: i32,
    y: i32,
    ts: i64,
}

fn window_json() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?)
        .join("Library/Application Support/sticky-todo/window.json"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn clamp_axis(value: i64, start: i32, span: u32, window_span: u32) -> i32 {
    let min = i64::from(start);
    let max = min + i64::from(span.saturating_sub(window_span));
    value.clamp(min, max).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

pub fn init(win: &tauri::Window) -> Result<(), Box<dyn std::error::Error>> {
    let size = win.outer_size()?;
    let primary = win.primary_monitor()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "primary monitor does not exist")
    })?;
    let saved = window_json()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<SavedPosition>(&bytes).ok());

    let mut restored = None;
    if let Some(saved) = saved {
        // 先按存档原样落位再校验：存档指向已断开的显示器时，clamp 必然修正
        // 坐标 → 判存档失效，回落主屏右下角默认。
        win.set_position(tauri::PhysicalPosition::new(saved.x, saved.y))?;
        if let Some(monitor) = win.current_monitor()? {
            let area = monitor.work_area();
            let x = clamp_axis(i64::from(saved.x), area.position.x, area.size.width, size.width);
            let y = clamp_axis(i64::from(saved.y), area.position.y, area.size.height, size.height);
            if saved.x == x && saved.y == y {
                restored = Some((i64::from(x), i64::from(y), *area));
            }
        }
    }

    let (x, y, area) = if let Some(position) = restored {
        position
    } else {
        let area = *primary.work_area();
        let margin = (24.0 * win.scale_factor()?).round() as i64;
        let x = i64::from(area.position.x) + i64::from(area.size.width)
            - i64::from(size.width) - margin;
        let y = i64::from(area.position.y) + i64::from(area.size.height)
            - i64::from(size.height) - margin;
        (x, y, area)
    };
    win.set_position(tauri::PhysicalPosition::new(
        clamp_axis(x, area.position.x, area.size.width, size.width),
        clamp_axis(y, area.position.y, area.size.height, size.height),
    ))?;
    Ok(())
}

pub fn save(win: &tauri::Window) -> Result<(), Box<dyn std::error::Error>> {
    let now = now_ms();
    if now - LAST_SAVE_MS.load(Ordering::Relaxed) < 1_000 {
        return Ok(());
    }
    let path = window_json().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "HOME is not set")
    })?;
    let position = win.outer_position()?;
    let saved = SavedPosition { x: position.x, y: position.y, ts: now };
    std::fs::create_dir_all(path.parent().expect("window.json has a parent"))?;
    std::fs::write(path, serde_json::to_vec(&saved)?)?;
    LAST_SAVE_MS.store(now, Ordering::Relaxed);
    Ok(())
}

pub fn note_moved(win: &tauri::Window) {
    if let Err(e) = save(win) {
        eprintln!("[shell] save sticky position failed: {e}");
    }
}
