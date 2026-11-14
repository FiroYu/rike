//! Windows 自动更新。
//!
//! GitHub 的资源下载会跨 API、github.com 与对象 CDN；这里手写有限次重定向，才能明确保证
//! Bearer 只出现在第一跳，绝不随 `Location` 泄漏到 CDN。`--update-wait` 必须在 `run()` 最顶部、
//! 任何 Tauri builder 和单实例插件之前等待旧进程，否则新进程可能被旧实例的聚焦回调吞掉。
//! release 使用 `panic=abort`，因此更新的网络、文件、进程与锁路径均以 `Result` 或容错分支收口，
//! 失败只更新状态并保留可启动的旧程序。
//!
//! v0.10.3 起：常驻循环（首查 +3s，瞬态失败退避 30s→2m→10m→30m，其余 1h 复查）、
//! 代理瞬断后直连回退一次（两种客户端仅代理配置不同）、错误分类（瞬态走中性
//! `retry_wait`，鉴权/结构类走红色 `error`），每次尝试追加 `%APPDATA%\sticky-todo\update.log`。

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use chrono::{Local, SecondsFormat};
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
/// 成功或非瞬态失败后的复查周期（顺带覆盖长运行机器的新版本发现）。
const RECHECK: Duration = Duration::from_secs(3600);
/// 自动检查瞬态失败的退避节奏；索引越界封顶在末档。
const BACKOFF_STEPS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// 网络/超时/5xx：退避自动重试，中性 UI。
    Retryable,
    /// GitHub 限流：按瞬态重试，文案单独标注。
    RateLimited,
    /// 404：最新版本接口不可达（公共仓无 release、资产缺失等）。
    AuthMissing,
    /// 401：令牌无效。
    AuthInvalid,
    /// 校验/换入/协议类结构问题：红色完整错误。
    Fatal,
}

impl FailureKind {
    fn is_retryable(self) -> bool {
        matches!(self, FailureKind::Retryable | FailureKind::RateLimited)
    }

    /// 面板主行的中文分类文案。
    fn label(self) -> &'static str {
        match self {
            FailureKind::Retryable => "网络或代理未就绪",
            FailureKind::RateLimited => "GitHub 限流",
            FailureKind::AuthMissing => "仓库无可用版本",
            FailureKind::AuthInvalid => "令牌无效",
            FailureKind::Fatal => "更新失败",
        }
    }

    /// 进状态与日志的分类标识。
    fn class(self) -> &'static str {
        match self {
            FailureKind::Retryable => "retryable",
            FailureKind::RateLimited => "rate_limited",
            FailureKind::AuthMissing => "auth_missing",
            FailureKind::AuthInvalid => "auth_invalid",
            FailureKind::Fatal => "fatal",
        }
    }
}

/// 检查/下载/换入任一环节的失败。`transport` = 请求发送层失败（代理瞬态类），
/// 是直连回退的唯一触发条件；HTTP 状态类与 body 类失败不回退。
#[derive(Debug, Clone)]
struct CheckError {
    kind: FailureKind,
    transport: bool,
    message: String,
}

impl CheckError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Fatal,
            transport: false,
            message: message.into(),
        }
    }

    fn with_kind(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            transport: false,
            message: message.into(),
        }
    }

    fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Retryable,
            transport: true,
            message: message.into(),
        }
    }

    /// 响应体读取/解析失败：多为截断类瞬态，自动重试，原始错误进日志。
    fn body(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Retryable,
            transport: false,
            message: message.into(),
        }
    }
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for CheckError {
    fn from(message: String) -> Self {
        Self::fatal(message)
    }
}

fn rate_limited(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        == Some("0")
        || headers.get("retry-after").is_some()
}

fn classify_status(status: reqwest::StatusCode, limited: bool) -> FailureKind {
    match status.as_u16() {
        401 => FailureKind::AuthInvalid,
        404 => FailureKind::AuthMissing,
        429 => FailureKind::RateLimited,
        403 if limited => FailureKind::RateLimited,
        500..=599 => FailureKind::Retryable,
        _ => FailureKind::Fatal,
    }
}

fn backoff_delay(step: usize) -> Duration {
    BACKOFF_STEPS[step.min(BACKOFF_STEPS.len() - 1)]
}

#[derive(Clone, Serialize)]
pub struct UpdateStatus {
    current: String,
    latest: Option<String>,
    state: String,
    message: Option<String>,
    last_check: Option<String>,
    next_retry: Option<String>,
    failure_class: Option<String>,
    detail: Option<String>,
}

impl UpdateStatus {
    fn new(state: &str, latest: Option<String>, message: Option<String>) -> Self {
        Self {
            current: current_version().to_owned(),
            latest,
            state: state.to_owned(),
            message,
            last_check: LAST_CHECK.lock().ok().and_then(|guard| guard.clone()),
            next_retry: None,
            failure_class: None,
            detail: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    url: String,
    size: u64,
}

static STATUS: LazyLock<Mutex<UpdateStatus>> =
    LazyLock::new(|| Mutex::new(UpdateStatus::new("idle", None, None)));
static CHECKING: AtomicBool = AtomicBool::new(false);
static LAST_CHECK: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn now_hhmm() -> String {
    Local::now().format("%H:%M").to_string()
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

fn tls_config() -> Result<rustls::ClientConfig, String> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| error.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

/// 代理版与直连版唯一差异是 `no_proxy`：TLS、重定向与超时配置完全复用（审核定案）。
fn build_client(direct: bool) -> Result<Client, String> {
    let mut builder = Client::builder()
        .tls_backend_preconfigured(tls_config()?)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(120));
    if direct {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|error| error.to_string())
}

fn get_follow_with(
    client: &Client,
    url: &str,
    token: &str,
    asset_download: bool,
) -> Result<Response, CheckError> {
    let mut target = reqwest::Url::parse(url)
        .map_err(|error| CheckError::fatal(error.to_string()))?;
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
        let response = request
            .send()
            .map_err(|error| CheckError::transport(error.to_string()))?;
        let status = response.status();
        if !status.is_redirection() {
            if status.is_client_error() || status.is_server_error() {
                let kind = classify_status(status, rate_limited(response.headers()));
                return Err(CheckError::with_kind(kind, format!("HTTP {status}")));
            }
            return Ok(response);
        }
        if !may_follow(hop) {
            return Err(CheckError::fatal("重定向次数超过上限"));
        }
        let location = response
            .headers()
            .get(LOCATION)
            .ok_or_else(|| CheckError::fatal("重定向缺少 Location"))?
            .to_str()
            .map_err(|_| CheckError::fatal("重定向 Location 无效"))?;
        target = target
            .join(location)
            .map_err(|error| CheckError::fatal(error.to_string()))?;
    }
    Err(CheckError::fatal("重定向次数超过上限"))
}

/// 代理优先，发送层失败后直连回退一次；两种客户端各构建一次复用整轮检查。
struct Fetcher {
    proxy: Client,
    direct: Option<Client>,
    fallback_used: bool,
}

impl Fetcher {
    fn new() -> Result<Self, CheckError> {
        Ok(Self {
            proxy: build_client(false).map_err(CheckError::fatal)?,
            direct: None,
            fallback_used: false,
        })
    }

    fn direct(&mut self) -> Result<&Client, CheckError> {
        if self.direct.is_none() {
            self.direct = Some(build_client(true).map_err(CheckError::fatal)?);
        }
        Ok(self.direct.as_ref().expect("直连客户端已就绪"))
    }

    fn get(&mut self, url: &str, token: &str, asset_download: bool) -> Result<Response, CheckError> {
        match get_follow_with(&self.proxy, url, token, asset_download) {
            Ok(response) => Ok(response),
            Err(error) if error.transport => {
                let direct = self.direct()?;
                match get_follow_with(direct, url, token, asset_download) {
                    Ok(response) => {
                        self.fallback_used = true;
                        Ok(response)
                    }
                    Err(second) => Err(CheckError {
                        kind: second.kind,
                        transport: false,
                        message: format!("proxy: {}; direct: {}", error.message, second.message),
                    }),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 本次检查实际使用的传输路径（日志用）。
    fn transport_label(&self) -> &'static str {
        if self.fallback_used {
            "proxy->direct"
        } else {
            "proxy"
        }
    }
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

/// 日志路径：`STICKY_QA_ROOT` 覆盖（e2e 里该键被 qa_session 剥掉，实际走隔离
/// APPDATA 下的生产路径）→ `%APPDATA%\sticky-todo\update.log`。
fn update_log_path() -> Option<PathBuf> {
    if let Ok(qa) = std::env::var("STICKY_QA_ROOT") {
        return Some(PathBuf::from(qa).join("update.log"));
    }
    std::env::var("APPDATA")
        .ok()
        .map(|appdata| PathBuf::from(appdata).join("sticky-todo").join("update.log"))
}

/// 超过 512KB 时仅保留末 200 行（临时文件 + 原子替换）。
fn rotate_log(path: &Path) {
    const MAX_BYTES: u64 = 512 * 1024;
    const KEEP_LINES: usize = 200;
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_BYTES {
        return;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    let keep_from = lines.len().saturating_sub(KEEP_LINES);
    let tmp = path.with_extension("log.tmp");
    if fs::write(&tmp, format!("{}\n", lines[keep_from..].join("\n"))).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// 每次尝试追加一行；错误经同步侧 sanitize 脱敏（令牌/URL userinfo），令牌本身不落盘。
fn log_attempt(transport: &str, token: Option<&str>, error: Option<&str>) {
    let Some(path) = update_log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let (state, latest) = STATUS
        .lock()
        .map(|guard| (guard.state.clone(), guard.latest.clone()))
        .unwrap_or_else(|_| ("unknown".into(), None));
    let mut line = format!(
        "{} check current={} latest={} outcome={} transport=",
        Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
        current_version(),
        latest.as_deref().unwrap_or("-"),
        state,
    );
    line.push_str(transport);
    if let Some(error) = error.filter(|error| !error.is_empty()) {
        let sanitized = crate::sched::sanitize_sync_error(error, token);
        line.push_str(&format!(" error=\"{sanitized}\""));
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
    rotate_log(&path);
}

fn read_response(response: Response, expected_size: u64) -> Result<Vec<u8>, CheckError> {
    let limit = expected_size
        .checked_add(1)
        .ok_or_else(|| CheckError::fatal("下载大小无效"))?;
    let mut bytes = Vec::new();
    response
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| CheckError::body(error.to_string()))?;
    if u64::try_from(bytes.len()).ok() != Some(expected_size) {
        return Err(CheckError::fatal("下载长度不匹配"));
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

/// 一次检查的网络与换入主体。成功路径已在内部落好 idle/downloading/ready/updated 状态。
fn perform(app: &tauri::AppHandle, fetcher: &mut Fetcher, token: &str) -> Result<(), CheckError> {
    let release_text = fetcher
        .get(&api_url(), token, false)?
        .text()
        .map_err(|error| CheckError::body(error.to_string()))?;
    let release: Release =
        serde_json::from_str(&release_text).map_err(|error| CheckError::body(error.to_string()))?;
    if !is_newer(&release.tag_name, current_version()) {
        set_status(
            Some(app),
            UpdateStatus::new("idle", Some(release.tag_name), None),
        );
        return Ok(());
    }
    let latest = release.tag_name.clone();
    let asset = pick_asset(&release.assets)
        .ok_or_else(|| CheckError::fatal("Release 缺少 app.exe"))?
        .clone();
    let checksum = release
        .assets
        .iter()
        .find(|candidate| candidate.name == "app.exe.sha256")
        .ok_or_else(|| CheckError::fatal("Release 缺少 app.exe.sha256"))?
        .clone();
    set_status(
        Some(app),
        UpdateStatus::new("downloading", Some(latest.clone()), None),
    );
    let bytes = read_response(fetcher.get(&asset.url, token, true)?, asset.size)
        .map_err(|error| CheckError {
            message: format!("app.exe {}", error.message),
            ..error
        })?;
    let sha_bytes = read_response(fetcher.get(&checksum.url, token, true)?, checksum.size)
        .map_err(|error| CheckError {
            message: format!("app.exe.sha256 {}", error.message),
            ..error
        })?;
    let sha_text = String::from_utf8(sha_bytes)
        .map_err(|_| CheckError::fatal("SHA-256 文件不是有效 UTF-8"))?;
    let expected = parse_sha256(&sha_text).ok_or_else(|| CheckError::fatal("SHA-256 文件格式无效"))?;
    if !verify_sha256(&bytes, &expected) {
        return Err(CheckError::fatal("app.exe SHA-256 校验失败"));
    }
    let new = write_staged(&bytes, &expected, &latest).map_err(CheckError::fatal)?;
    set_status(
        Some(app),
        UpdateStatus::new("ready", Some(latest.clone()), None),
    );
    apply(app, &new, &latest).map_err(CheckError::fatal)
}

/// 自动检查的裁决：Transient → 调度器按退避节奏重试；Settled → 按 1h 周期复查。
enum Verdict {
    Settled,
    Transient,
}

/// 单次更新检查。`backoff` 是本次瞬态失败后调度器将采用的等待时长（用于展示重试时刻）。
/// 与其他检查共用 CHECKING 锁：锁被占时直接让位，由进行中的检查产出状态。
fn check_once(app: &tauri::AppHandle, automatic: bool, backoff: Duration) -> Verdict {
    if CHECKING.swap(true, Ordering::SeqCst) {
        return Verdict::Settled;
    }
    if let Ok(mut last) = LAST_CHECK.lock() {
        *last = Some(now_hhmm());
    }
    let verdict = run_check(app, automatic, backoff);
    CHECKING.store(false, Ordering::SeqCst);
    verdict
}

fn run_check(app: &tauri::AppHandle, automatic: bool, backoff: Duration) -> Verdict {
    // 公开仓匿名即可检查与下载；令牌（Git 凭据或面板 PAT）仅私有部署需要。
    let token = token().unwrap_or_default();
    set_status(Some(app), UpdateStatus::new("checking", None, None));
    let mut fetcher = match Fetcher::new() {
        Ok(fetcher) => fetcher,
        Err(error) => {
            return fail(app, automatic, backoff, &token, "none", &error);
        }
    };
    let result = perform(app, &mut fetcher, &token);
    match result {
        Ok(()) => {
            log_attempt(fetcher.transport_label(), Some(&token), None);
            Verdict::Settled
        }
        Err(error) => fail(app, automatic, backoff, &token, fetcher.transport_label(), &error),
    }
}

/// 失败收口：清理暂存、按自动/手动与分类落状态、写日志，并给出重试裁决。
/// 自动 + 瞬态 → `retry_wait`（中性）；其余 → `error`（红色，手动检查同此）。
fn fail(
    app: &tauri::AppHandle,
    automatic: bool,
    backoff: Duration,
    token: &str,
    transport: &str,
    error: &CheckError,
) -> Verdict {
    if let Ok(dir) = update_dir() {
        let _ = fs::remove_file(dir.join("app.exe.part"));
    }
    let sanitized = crate::sched::sanitize_sync_error(&error.message, Some(token));
    if automatic && error.kind.is_retryable() {
        let next = Local::now()
            + chrono::Duration::from_std(backoff)
                .unwrap_or_else(|_| chrono::Duration::zero());
        let next_hhmm = next.format("%H:%M").to_string();
        let base = match error.kind {
            FailureKind::RateLimited => "GitHub 限流",
            _ => "网络未就绪",
        };
        let mut status = UpdateStatus::new(
            "retry_wait",
            None,
            Some(format!("{base}，{next_hhmm} 自动重试")),
        );
        status.next_retry = Some(next_hhmm);
        status.failure_class = Some(error.kind.class().to_owned());
        status.detail = Some(sanitized.clone());
        set_status(Some(app), status);
        log_attempt(transport, Some(token), Some(&sanitized));
        return Verdict::Transient;
    }
    let mut status = UpdateStatus::new("error", None, Some(error.kind.label().to_owned()));
    status.failure_class = Some(error.kind.class().to_owned());
    status.detail = Some(sanitized.clone());
    set_status(Some(app), status);
    log_attempt(transport, Some(token), Some(&sanitized));
    Verdict::Settled
}

/// 常驻循环：首查 +3s；瞬态失败退避重试；其余按 1h 周期复查。
fn update_loop(app: tauri::AppHandle) {
    std::thread::sleep(Duration::from_secs(3));
    let mut step = 0_usize;
    loop {
        let backoff = backoff_delay(step);
        match check_once(&app, true, backoff) {
            Verdict::Transient => {
                step = (step + 1).min(BACKOFF_STEPS.len() - 1);
                std::thread::sleep(backoff);
            }
            Verdict::Settled => {
                step = 0;
                std::thread::sleep(RECHECK);
            }
        }
    }
}

pub fn spawn(app: tauri::AppHandle) {
    if enabled() {
        std::thread::spawn(move || update_loop(app));
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
    std::thread::spawn(move || check_once(&app, false, Duration::ZERO));
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

    #[test]
    fn status_codes_classify_into_failure_kinds() {
        use reqwest::StatusCode;
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, false),
            FailureKind::AuthInvalid
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, false),
            FailureKind::AuthMissing
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, false),
            FailureKind::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN, true),
            FailureKind::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN, false),
            FailureKind::Fatal
        );
        assert_eq!(
            classify_status(StatusCode::BAD_GATEWAY, false),
            FailureKind::Retryable
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST, false),
            FailureKind::Fatal
        );
    }

    #[test]
    fn rate_limit_headers_are_detected() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert!(!rate_limited(&headers));
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        assert!(rate_limited(&headers));
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "60".parse().unwrap());
        assert!(rate_limited(&headers));
    }

    #[test]
    fn backoff_escalates_and_caps() {
        assert_eq!(backoff_delay(0), Duration::from_secs(30));
        assert_eq!(backoff_delay(1), Duration::from_secs(120));
        assert_eq!(backoff_delay(2), Duration::from_secs(600));
        assert_eq!(backoff_delay(3), Duration::from_secs(1800));
        assert_eq!(backoff_delay(99), Duration::from_secs(1800));
    }

    #[test]
    fn transport_errors_are_marked_for_fallback() {
        let error = CheckError::transport("connection refused");
        assert!(error.transport);
        assert!(error.kind.is_retryable());
        assert!(!CheckError::fatal("协议坏").transport);
        assert!(!CheckError::body("截断").transport);
        assert!(CheckError::body("截断").kind.is_retryable());
        assert!(!CheckError::fatal("校验").kind.is_retryable());
    }

    #[test]
    fn failure_labels_and_classes_cover_all_kinds() {
        for kind in [
            FailureKind::Retryable,
            FailureKind::RateLimited,
            FailureKind::AuthMissing,
            FailureKind::AuthInvalid,
            FailureKind::Fatal,
        ] {
            assert!(!kind.label().is_empty());
            assert!(!kind.class().is_empty());
        }
        assert!(FailureKind::RateLimited.is_retryable());
        assert!(!FailureKind::AuthInvalid.is_retryable());
    }

    #[test]
    fn direct_and_proxy_clients_both_construct() {
        assert!(build_client(false).is_ok());
        assert!(build_client(true).is_ok());
    }

    #[test]
    fn log_rotation_keeps_last_200_lines() {
        let path = std::env::temp_dir().join(format!(
            "sticky-update-log-test-{}.log",
            std::process::id()
        ));
        let mut content = String::new();
        for index in 0..5000 {
            content.push_str(&format!("line-{index:04} {}\n", "x".repeat(100)));
        }
        fs::write(&path, &content).unwrap();
        rotate_log(&path);
        let rotated = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = rotated.lines().collect();
        assert_eq!(lines.len(), 200);
        assert!(lines[0].starts_with("line-4800"), "first kept: {}", lines[0]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn log_rotation_ignores_small_logs() {
        let path = std::env::temp_dir().join(format!(
            "sticky-update-log-small-{}.log",
            std::process::id()
        ));
        fs::write(&path, "keep-a\nkeep-b\n").unwrap();
        rotate_log(&path);
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep-a\nkeep-b\n");
        let _ = fs::remove_file(&path);
    }
}
