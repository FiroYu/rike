//! Windows 自动更新。
//!
//! GitHub 的资源下载会跨 API、github.com 与对象 CDN；这里手写有限次重定向，才能明确保证
//! Bearer 只出现在第一跳，绝不随 `Location` 泄漏到 CDN。`--update-wait` 必须在 `run()` 最顶部、
//! 任何 Tauri builder 和单实例插件之前等待旧进程，否则新进程可能被旧实例的聚焦回调吞掉。
//! release 使用 `panic=abort`，因此更新的网络、文件、进程与锁路径均以 `Result` 或容错分支收口，
//! 失败只更新状态并保留可启动的旧程序。

use std::fs;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, LOCATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Emitter;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

use crate::store::CmdError;

const API_PATH: &str = "/repos/FiroYu/rike/releases/latest";
const MAX_REDIRECTS: usize = 5;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_FAILED: u32 = 0xffff_ffff;

#[derive(Clone, Serialize)]
pub struct UpdateStatus {
    current: String,
    latest: Option<String>,
    state: String,
    message: Option<String>,
}

impl UpdateStatus {
    fn new(state: &str, latest: Option<String>, message: Option<String>) -> Self {
        Self {
            current: current_version().to_owned(),
            latest,
            state: state.to_owned(),
            message,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    url: String,
    size: u64,
}

static STATUS: LazyLock<Mutex<UpdateStatus>> =
    LazyLock::new(|| Mutex::new(UpdateStatus::new("idle", None, None)));
static CHECKING: AtomicBool = AtomicBool::new(false);

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn parse_ver(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_ver(candidate), parse_ver(current)), (Some(a), Some(b)) if a > b)
}

fn pick_asset(assets: &[Asset]) -> Option<&Asset> {
    assets.iter().find(|asset| asset.name == "app.exe")
}

fn parse_sha256(text: &str) -> Option<[u8; 32]> {
    let token = text.split_whitespace().next()?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut result = [0_u8; 32];
    for (index, pair) in token.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        result[index] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(result)
}

fn verify_sha256(bytes: &[u8], expected: &[u8; 32]) -> bool {
    Sha256::digest(bytes).as_slice() == expected
}

fn auth_for(hop: usize, host: &str) -> bool {
    hop == 0 && matches!(host, "api.github.com" | "github.com")
}

fn asset_headers(hop: usize, host: &str, token: &str) -> Vec<(&'static str, String)> {
    if auth_for(hop, host) && !token.is_empty() {
        vec![
            (AUTHORIZATION.as_str(), format!("Bearer {token}")),
            (ACCEPT.as_str(), "application/octet-stream".to_owned()),
        ]
    } else {
        Vec::new()
    }
}

fn may_follow(hops: usize) -> bool {
    hops < MAX_REDIRECTS
}

fn client() -> Result<Client, String> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| error.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Client::builder()
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())
}

fn get_follow_with(
    client: &Client,
    url: &str,
    token: &str,
    asset_download: bool,
) -> Result<Response, String> {
    let mut target = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    for hop in 0..=MAX_REDIRECTS {
        let mut request = client
            .get(target.clone())
            .header(USER_AGENT, "sticky-todo-updater")
            .header("X-GitHub-Api-Version", "2022-11-28");
        let host = target.host_str().unwrap_or_default();
        if asset_download {
            for (name, value) in asset_headers(hop, host, token) {
                request = request.header(name, value);
            }
        } else if auth_for(hop, host) && !token.is_empty() {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = request.send().map_err(|error| error.to_string())?;
        if !response.status().is_redirection() {
            return response
                .error_for_status()
                .map_err(|error| error.to_string());
        }
        if !may_follow(hop) {
            return Err("重定向次数超过上限".into());
        }
        let location = response
            .headers()
            .get(LOCATION)
            .ok_or_else(|| "重定向缺少 Location".to_owned())?
            .to_str()
            .map_err(|_| "重定向 Location 无效".to_owned())?;
        target = target.join(location).map_err(|error| error.to_string())?;
    }
    Err("重定向次数超过上限".into())
}

fn get_follow(client: &Client, url: &str, token: &str) -> Result<Response, String> {
    get_follow_with(client, url, token, false)
}

fn get_asset(client: &Client, url: &str, token: &str) -> Result<Response, String> {
    get_follow_with(client, url, token, true)
}

fn credential_token() -> Option<String> {
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    if stdin
        .write_all(b"protocol=https\nhost=github.com\n\n")
        .is_err()
    {
        return None;
    }
    drop(stdin);
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix("password=")
                .filter(|value| !value.is_empty())
        })
        .map(ToOwned::to_owned)
}

fn token() -> Option<String> {
    credential_token().or_else(crate::sync_config::update_pat)
}

fn update_dir() -> Result<PathBuf, String> {
    crate::store::default_repo_dir()
        .parent()
        .map(|parent| parent.join("update"))
        .ok_or_else(|| "无法确定更新暂存目录".to_owned())
}

fn api_url() -> String {
    let base =
        std::env::var("STICKY_UPDATE_URL").unwrap_or_else(|_| "https://api.github.com".to_owned());
    format!("{}{}", base.trim_end_matches('/'), API_PATH)
}

fn enabled() -> bool {
    if std::env::var("STICKY_UPDATE_DISABLE").as_deref() == Ok("1") {
        return false;
    }
    !cfg!(debug_assertions) || std::env::var("STICKY_UPDATE_FORCE").as_deref() == Ok("1")
}

fn set_status(app: Option<&tauri::AppHandle>, status: UpdateStatus) {
    if let Ok(mut stored) = STATUS.lock() {
        *stored = status.clone();
    } else {
        eprintln!("[update] 状态锁不可用");
    }
    if let Some(app) = app {
        if let Err(error) = app.emit("update-status", status) {
            eprintln!("[update] 发送状态失败: {error}");
        }
    }
}

fn read_response(response: Response, expected_size: u64) -> Result<Vec<u8>, String> {
    let limit = expected_size
        .checked_add(1)
        .ok_or_else(|| "下载大小无效".to_owned())?;
    let mut bytes = Vec::new();
    response
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if u64::try_from(bytes.len()).ok() != Some(expected_size) {
        return Err("下载长度不匹配".into());
    }
    Ok(bytes)
}

fn write_staged(bytes: &[u8], expected: &[u8; 32], version: &str) -> Result<PathBuf, String> {
    let dir = update_dir()?;
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let part = dir.join("app.exe.part");
    let new = dir.join("app.exe.new");
    let result = (|| {
        let mut file = fs::File::create(&part).map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&part, &new).map_err(|error| error.to_string())?;
        fs::write(dir.join("app.exe.sha256"), hex_sha(expected))
            .map_err(|error| error.to_string())?;
        fs::write(dir.join("version"), version).map_err(|error| error.to_string())?;
        Ok(new.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
        let _ = fs::remove_file(&new);
    }
    result
}

fn hex_sha(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn move_replace(from: &Path, to: &Path) -> Result<(), String> {
    let result = unsafe {
        MoveFileExW(
            wide(from).as_ptr(),
            wide(to).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

fn replace_exe(new: &Path) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let old = exe.with_file_name("app.exe.old");
    let _ = fs::remove_file(&old);
    fs::rename(&exe, &old).map_err(|error| error.to_string())?;
    if let Err(error) = move_replace(new, &exe) {
        if let Err(rollback) = move_replace(&old, &exe) {
            eprintln!("[update] 换入失败且回滚失败: {rollback}");
        }
        return Err(format!("换入新版失败: {error}"));
    }
    Ok(exe)
}

fn launch_replacement(exe: &Path) -> Result<(), String> {
    Command::new(exe)
        .args(["--update-wait", &std::process::id().to_string()])
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn apply(app: &tauri::AppHandle, new: &Path, latest: &str) -> Result<(), String> {
    if crate::board_window_zoomed() {
        set_status(
            Some(app),
            UpdateStatus::new(
                "ready",
                Some(latest.to_owned()),
                Some("已就绪，稍后重启".into()),
            ),
        );
        return Ok(());
    }
    let exe = replace_exe(new)?;
    if let Err(error) = launch_replacement(&exe) {
        eprintln!("[update] 新版启动失败: {error}");
        return Err(error);
    }
    set_status(
        Some(app),
        UpdateStatus::new("updated", Some(latest.to_owned()), None),
    );
    app.exit(0);
    Ok(())
}

fn check(app: tauri::AppHandle, delay: bool) {
    if CHECKING.swap(true, Ordering::SeqCst) {
        return;
    }
    if delay {
        std::thread::sleep(Duration::from_secs(3));
    }
    let result = (|| -> Result<(), String> {
        // 公开仓匿名即可检查与下载；令牌（Git 凭据或面板 PAT）仅私有部署需要。
        let token = token().unwrap_or_default();
        set_status(Some(&app), UpdateStatus::new("checking", None, None));
        let client = client()?;
        let release_text = get_follow(&client, &api_url(), &token)?
            .text()
            .map_err(|error| error.to_string())?;
        let release: Release =
            serde_json::from_str(&release_text).map_err(|error| error.to_string())?;
        if !is_newer(&release.tag_name, current_version()) {
            set_status(
                Some(&app),
                UpdateStatus::new("idle", Some(release.tag_name), None),
            );
            return Ok(());
        }
        let latest = release.tag_name.clone();
        let asset = pick_asset(&release.assets).ok_or_else(|| "Release 缺少 app.exe".to_owned())?;
        let checksum = release
            .assets
            .iter()
            .find(|candidate| candidate.name == "app.exe.sha256")
            .ok_or_else(|| "Release 缺少 app.exe.sha256".to_owned())?;
        set_status(
            Some(&app),
            UpdateStatus::new("downloading", Some(latest.clone()), None),
        );
        let bytes = read_response(get_asset(&client, &asset.url, &token)?, asset.size)
            .map_err(|error| format!("app.exe {error}"))?;
        let sha_bytes = read_response(get_asset(&client, &checksum.url, &token)?, checksum.size)
            .map_err(|error| format!("app.exe.sha256 {error}"))?;
        let sha_text =
            String::from_utf8(sha_bytes).map_err(|_| "SHA-256 文件不是有效 UTF-8".to_owned())?;
        let expected = parse_sha256(&sha_text).ok_or_else(|| "SHA-256 文件格式无效".to_owned())?;
        if !verify_sha256(&bytes, &expected) {
            return Err("app.exe SHA-256 校验失败".into());
        }
        let new = write_staged(&bytes, &expected, &latest)?;
        set_status(
            Some(&app),
            UpdateStatus::new("ready", Some(latest.clone()), None),
        );
        apply(&app, &new, &latest)
    })();
    if let Err(error) = result {
        eprintln!("[update] 更新失败: {error}");
        if let Ok(dir) = update_dir() {
            let _ = fs::remove_file(dir.join("app.exe.part"));
        }
        set_status(Some(&app), UpdateStatus::new("error", None, Some(error)));
    }
    CHECKING.store(false, Ordering::SeqCst);
}

pub fn spawn(app: tauri::AppHandle) {
    if enabled() {
        std::thread::spawn(move || check(app, true));
    }
}

#[tauri::command]
pub fn update_status() -> UpdateStatus {
    STATUS
        .lock()
        .map(|status| status.clone())
        .unwrap_or_else(|_| UpdateStatus::new("error", None, Some("状态不可用".into())))
}

#[tauri::command]
pub fn update_check_now(app: tauri::AppHandle) -> Result<(), CmdError> {
    if !enabled() {
        return Err(CmdError::bad_request("当前构建未启用自动更新"));
    }
    std::thread::spawn(move || check(app, false));
    Ok(())
}

/// 新进程先等待旧进程退出；任何参数或 Win32 错误都只导致继续普通启动。
pub fn early_restarter() {
    let args: Vec<String> = std::env::args().collect();
    let Some(index) = args.iter().position(|arg| arg == "--update-wait") else {
        return;
    };
    let Some(pid) = args.get(index + 1).and_then(|arg| arg.parse::<u32>().ok()) else {
        return;
    };
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return;
    }
    let wait = unsafe { WaitForSingleObject(handle, 30_000) };
    unsafe { CloseHandle(handle) };
    if wait == WAIT_FAILED {
        eprintln!("[update] 等待旧进程失败");
    }
}

/// 启动期清理旧映像，并续用上次因放大编辑而留下且仍通过校验的 `.new`。
pub fn cleanup_old() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = fs::remove_file(exe.with_file_name("app.exe.old"));
    let Ok(dir) = update_dir() else {
        return;
    };
    let new = dir.join("app.exe.new");
    if !new.exists() {
        return;
    }
    let validation = (|| -> Result<(), String> {
        let version = fs::read_to_string(dir.join("version")).map_err(|error| error.to_string())?;
        if !is_newer(version.trim(), current_version()) {
            return Err("暂存版本不比当前版本新".into());
        }
        let expected_text =
            fs::read_to_string(dir.join("app.exe.sha256")).map_err(|error| error.to_string())?;
        let expected =
            parse_sha256(&expected_text).ok_or_else(|| "暂存 SHA-256 无效".to_owned())?;
        let bytes = fs::read(&new).map_err(|error| error.to_string())?;
        if !verify_sha256(&bytes, &expected) {
            return Err("暂存 app.exe 校验失败".into());
        }
        Ok(())
    })();
    if let Err(error) = validation {
        eprintln!("[update] 暂存更新校验失败，删除 .new: {error}");
        let _ = fs::remove_file(new);
        return;
    }
    let result = replace_exe(&new).and_then(|exe| launch_replacement(&exe));
    if let Err(error) = result {
        eprintln!("[update] 应用暂存更新失败，保留 .new 供下次启动重试: {error}");
        return;
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_strict_three_part_numbers() {
        assert_eq!(parse_ver("v0.10.0"), Some((0, 10, 0)));
        assert!(is_newer("v0.10.0", "0.9.7"));
        assert!(!is_newer("0.9.7", "0.9.7"));
        assert!(!is_newer("0.10", "0.9.7"));
        assert!(!is_newer("乱码", "0.9.7"));
    }

    #[test]
    fn exact_executable_asset_is_selected() {
        let assets = vec![
            Asset {
                name: "setup.exe".into(),
                url: String::new(),
                size: 1,
            },
            Asset {
                name: "app.exe".into(),
                url: String::new(),
                size: 2,
            },
        ];
        assert_eq!(pick_asset(&assets).map(|asset| asset.size), Some(2));
    }

    #[test]
    fn checksum_parser_accepts_sha256sum_and_rejects_garbage() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(parse_sha256(&format!("{hash}  app.exe")).is_some());
        assert!(parse_sha256(&format!("{hash}  app.exe\n")).is_some());
        assert!(parse_sha256("not-a-hash  app.exe").is_none());
    }

    #[test]
    fn known_sha256_vector_verifies() {
        let expected =
            parse_sha256("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .expect("向量有效");
        assert!(verify_sha256(b"abc", &expected));
        assert!(!verify_sha256(b"abd", &expected));
    }

    #[test]
    fn authorization_is_only_for_first_github_hop() {
        assert!(auth_for(0, "api.github.com"));
        assert!(auth_for(0, "github.com"));
        assert!(!auth_for(1, "github.com"));
        assert!(!auth_for(0, "objects.githubusercontent.com"));
    }

    #[test]
    fn asset_download_headers_only_appear_on_first_github_hop() {
        let expected = vec![
            ("authorization", "Bearer secret".to_owned()),
            ("accept", "application/octet-stream".to_owned()),
        ];
        assert_eq!(asset_headers(0, "api.github.com", "secret"), expected);
        assert_eq!(asset_headers(0, "github.com", "secret"), expected);
        assert!(asset_headers(1, "api.github.com", "secret").is_empty());
        assert!(asset_headers(0, "objects.githubusercontent.com", "secret").is_empty());
        // 匿名（公开仓）：任何跳数都不带 Authorization。
        assert!(asset_headers(0, "api.github.com", "").is_empty());
    }

    #[test]
    fn redirects_stop_at_five_hops() {
        assert!(may_follow(4));
        assert!(!may_follow(5));
    }
}
