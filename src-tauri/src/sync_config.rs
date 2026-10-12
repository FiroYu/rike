use std::{fs, io::Write, path::PathBuf, sync::RwLock};

use crate::store::{CmdError, REPO_URL};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct SyncConfig {
    repo_url: String,
    pat: String,
    #[serde(default)]
    update_pat: String,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            repo_url: REPO_URL.into(),
            pat: String::new(),
            update_pat: String::new(),
        }
    }
}

#[derive(serde::Serialize)]
pub struct SyncConfigView {
    pub repo_url: String,
    pub pat_set: bool,
    pub update_pat_set: bool,
}

static CACHE: RwLock<Option<SyncConfig>> = RwLock::new(None);

fn config_path() -> Result<PathBuf, CmdError> {
    crate::store::default_repo_dir()
        .parent()
        .map(|dir| dir.join("app_config.json"))
        .ok_or_else(config_error)
}

fn config_error() -> CmdError {
    CmdError::internal("无法读取或保存同步设置")
}

// Only credential-free GitHub repository URLs may be displayed or persisted.
fn valid_repo_url(url: &str) -> bool {
    let Some(path) = url.strip_prefix("https://github.com/") else {
        return false;
    };
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
}

fn read_config() -> Result<SyncConfig, CmdError> {
    let config: SyncConfig = match fs::read(config_path()?) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| config_error())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SyncConfig::default(),
        Err(_) => return Err(config_error()),
    };
    if !valid_repo_url(&config.repo_url)
        || (!config.pat.is_empty() && config.repo_url.contains(&config.pat))
    {
        return Err(config_error());
    }
    Ok(config)
}

#[cfg(target_os = "android")]
pub(crate) fn pat() -> Option<String> {
    if let Some(config) = CACHE
        .read()
        .ok()
        .or_else(|| {
            log::error!(target: "sticky", "[sync_config] PAT cache read lock poisoned");
            None
        })?
        .as_ref()
    {
        log::info!(target: "sticky", "[sync_config] PAT source=cache");
        return (!config.pat.is_empty()).then(|| config.pat.clone());
    }
    let mut cache = CACHE.write().ok().or_else(|| {
        log::error!(target: "sticky", "[sync_config] PAT cache write lock poisoned");
        None
    })?;
    if cache.is_none() {
        log::info!(target: "sticky", "[sync_config] PAT source=file (default if absent)");
        *cache = Some(read_config().ok().or_else(|| {
            log::error!(target: "sticky", "[sync_config] PAT file read/validation failed");
            None
        })?);
    } else {
        log::info!(target: "sticky", "[sync_config] PAT source=cache (filled while waiting)");
    }
    cache
        .as_ref()
        .and_then(|config| (!config.pat.is_empty()).then(|| config.pat.clone()))
}

#[tauri::command]
pub fn get_sync_config() -> Result<SyncConfigView, CmdError> {
    let _guard = CACHE.read().map_err(|_| config_error())?;
    let config = read_config()?;
    Ok(SyncConfigView {
        repo_url: config.repo_url,
        pat_set: !config.pat.is_empty(),
        update_pat_set: !config.update_pat.is_empty(),
    })
}

#[cfg(windows)]
pub(crate) fn update_pat() -> Option<String> {
    read_config()
        .ok()
        .and_then(|config| (!config.update_pat.is_empty()).then_some(config.update_pat))
}

#[tauri::command]
pub fn set_sync_config(
    repo_url: Option<String>,
    pat: Option<String>,
    update_pat: Option<String>,
) -> Result<(), CmdError> {
    let mut cache = CACHE.write().map_err(|_| config_error())?;
    let mut config = read_config()?;
    if let Some(url) = repo_url {
        if !valid_repo_url(&url) {
            return Err(CmdError::bad_request(
                "请使用不含凭据的 GitHub HTTPS 仓库地址",
            ));
        }
        config.repo_url = url;
    }
    if let Some(pat) = pat {
        config.pat = pat;
    }
    if let Some(update_pat) = update_pat {
        config.update_pat = update_pat;
    }
    if !config.pat.is_empty() && config.repo_url.contains(&config.pat) {
        return Err(config_error());
    }
    let path = config_path()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| config_error())?
        .as_nanos();
    let tmp = path.with_extension(format!("{}.{stamp}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&config).map_err(|_| config_error())?;
    let result = (|| -> std::io::Result<()> {
        fs::create_dir_all(path.parent().ok_or(std::io::ErrorKind::NotFound)?)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            };
            let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
            let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe {
                MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(not(windows))]
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|_| config_error())?;
    *cache = None;
    #[cfg(target_os = "android")]
    log::info!(target: "sticky", "[sync_config] saved; PAT cache invalidated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SyncConfig;

    #[test]
    fn old_config_without_update_pat_round_trips() {
        let old = r#"{"repo_url":"https://github.com/your-account/sticky-sync.git","pat":"secret"}"#;
        let config: SyncConfig = serde_json::from_str(old).expect("旧配置应可解析");
        assert!(config.update_pat.is_empty());
        let encoded = serde_json::to_string(&config).expect("配置应可序列化");
        let decoded: SyncConfig = serde_json::from_str(&encoded).expect("配置应可往返");
        assert_eq!(decoded.update_pat, "");
        assert_eq!(decoded.pat, "secret");
    }
}
