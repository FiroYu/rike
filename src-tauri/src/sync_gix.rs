//! Android and host-test Git backend. No Git executable or CA files are used.
//!
//! gix has no receive-pack client. For this small journal, push sends all objects
//! reachable from the tip, without delta negotiation. The receiver deduplicates.
//! Diverged history is collapsed into one replay commit on the upstream after a
//! three-way tree merge; journal content matters, not the original commit shape.

use super::{is_push_rejected, repository_id, GitBackend, SyncError};
use gix::{bstr::{BString, ByteSlice}, ObjectId, Repository};
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit};
use gix::protocol::transport::{self, client::blocking_io::http};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs, io::{self, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

type Result<T> = std::result::Result<T, SyncError>;
type Files = BTreeMap<String, (gix::objs::tree::EntryKind, Vec<u8>)>;
const MAIN: &str = "refs/heads/main";
const MAX_HTTP_BYTES: u64 = 64 * 1024 * 1024;

pub struct GixBackend {
    pat_provider: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

fn git(error: impl std::fmt::Display) -> SyncError { SyncError::Git(error.to_string()) }

fn safe_message(message: &str, pat: Option<&str>) -> String {
    // Servers can echo an Authorization value as well as the original PAT.
    // Redact before the scheduler's length limit so partial secrets never leak.
    let message = match pat.filter(|p| !p.is_empty()) {
        Some(pat) => message.replace(&basic_credential(pat), "[redacted]"),
        None => message.to_owned(),
    };
    crate::sched::sanitize_sync_error(&message, pat)
}

fn basic_credential(pat: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in format!("pat:{pat}").as_bytes().chunks(3) {
        let a = chunk[0] as usize;
        let b = chunk.get(1).copied().unwrap_or(0) as usize;
        let c = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(ALPHABET[a >> 2] as char);
        out.push(ALPHABET[((a & 3) << 4) | (b >> 4)] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((b & 15) << 2) | (c >> 6)] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[c & 63] as char } else { '=' });
    }
    out
}

impl GixBackend {
    pub fn new(pat_provider: Arc<dyn Fn() -> Option<String> + Send + Sync>) -> Self {
        Self { pat_provider }
    }

    fn operation<T>(&self, stage: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        log::info!(target: "sticky", "[sync] {stage} start");
        let result = f().map_err(|error| {
            let pat = (self.pat_provider)();
            let clean = |s: String| safe_message(&s, pat.as_deref());
            // GitUnavailable is reserved for the desktop CLI. Preserve all
            // variants at this boundary; library, I/O and protocol errors are Git.
            match error {
                SyncError::Git(s) => SyncError::Git(clean(s)),
                SyncError::Conflict(s) => SyncError::Conflict(clean(s)),
                SyncError::GitUnavailable(s) => SyncError::GitUnavailable(clean(s)),
            }
        });
        if let Err(error) = &result {
            log::error!(target: "sticky", "[sync] {stage} failed: {error}");
        }
        log::info!(target: "sticky", "[sync] {stage} end ok={}", result.is_ok());
        result
    }

    fn pat(&self) -> Option<String> {
        let pat = (self.pat_provider)().filter(|p| !p.is_empty());
        log::info!(target: "sticky", "[sync] PAT available={}", pat.is_some());
        pat
    }

    fn client() -> Result<reqwest::blocking::Client> {
        // Explicit provider and verifier: no process-wide TLS initialization,
        // platform verifier, native roots, Android JNI or certificate files.
        let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().map_err(git)?
            .with_root_certificates(roots).with_no_client_auth();
        reqwest::blocking::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(120))
            .build().map_err(git)
    }

    fn local_path(url: &str) -> Result<Option<PathBuf>> {
        if url.starts_with("file:") {
            return reqwest::Url::parse(url).map_err(git)?.to_file_path()
                .map(Some).map_err(|_| git("无效的 file:// 仓库 URL"));
        }
        Ok(Path::new(url).exists().then(|| PathBuf::from(url)))
    }

    fn normalize_url(url: &str) -> Result<String> {
        if let Some(path) = Self::local_path(url)? {
            let path = fs::canonicalize(path).map_err(git)?;
            return reqwest::Url::from_file_path(path).map(|u| u.to_string())
                .map_err(|_| git("本地仓库路径无法转换为 URL"));
        }
        Ok(url.to_owned())
    }

    fn network_url(url: &str) -> Result<reqwest::Url> {
        let parsed = reqwest::Url::parse(url).map_err(git)?;
        let mut allowed = parsed.scheme() == "https";
        // Plain HTTP is ONLY available to the in-process loopback mock tests.
        #[cfg(test)]
        { allowed |= parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1"); }
        if !allowed || !parsed.username().is_empty() || parsed.password().is_some()
            || parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(git("同步只支持无内嵌凭据的 HTTPS URL 或本地路径"));
        }
        Ok(parsed)
    }

    fn open(path: &Path) -> Result<Repository> {
        let repo = gix::open(path).map_err(git)?;
        if repo.is_bare() { return Err(git("同步目录必须包含工作区")); }
        Ok(repo)
    }

    fn config_value(repo: &Repository, key: &str) -> Option<String> {
        repo.config_snapshot().string(key).map(|v| v.to_string())
    }

    fn configure(repo: &Repository, changes: &[(&str, &str)]) -> Result<()> {
        let path = repo.common_dir().join("config");
        let mut lock = FileLock::new(&path)?;
        let mut config = gix::config::File::from_path_no_includes(path.clone(), gix::config::Source::Local).map_err(git)?;
        for (key, value) in changes {
            config.set_raw_value(*key, *value).map_err(git)?;
        }
        // Old installations may have persisted the obsolete per-repo CA path.
        if let Ok(mut values) = config.raw_values_mut("http.sslCAInfo") { values.delete_all(); }
        config.write_to(lock.file.as_mut().expect("live lock")).map_err(git)?;
        lock.commit()
    }

    fn protect(repo: &Repository) -> Result<()> {
        Self::configure(repo, &[("core.autocrlf", "false"), ("core.eol", "lf")])
    }

    fn branch(repo: &Repository) -> Result<String> {
        repo.head_name().map_err(git)?.and_then(|n| n.as_bstr().to_str().ok().map(str::to_owned))
            .and_then(|n| n.strip_prefix("refs/heads/").map(str::to_owned))
            .ok_or_else(|| git("工作日志 HEAD 必须指向本地分支"))
    }

    fn set_head(repo: &Repository, name: &str) -> Result<()> {
        let signature = Self::identity(repo);
        repo.edit_references_as([RefEdit {
            name: "HEAD".try_into().map_err(git)?, deref: false,
            change: Change::Update {
                log: LogChange::default(), expected: PreviousValue::Any,
                new: gix::refs::Target::Symbolic(name.try_into().map_err(git)?),
            },
        }], Some(signature.to_ref(&mut Default::default()))).map_err(git)?;
        Ok(())
    }

    fn update_ref(repo: &Repository, name: &str, id: ObjectId, previous: PreviousValue, message: &str) -> Result<()> {
        let signature = Self::identity(repo);
        repo.edit_references_as([RefEdit {
            name: name.try_into().map_err(git)?, deref: false,
            change: Change::Update {
                log: LogChange { message: message.into(), ..Default::default() },
                expected: previous, new: id.into(),
            },
        }], Some(signature.to_ref(&mut Default::default()))).map_err(git)?;
        Ok(())
    }

    fn tip(repo: &Repository, name: &str) -> Result<Option<ObjectId>> {
        repo.try_find_reference(name).map_err(git)?
            .map(|mut r| r.peel_to_id().map(|id| id.detach()).map_err(git)).transpose()
    }

    fn head(repo: &Repository) -> Result<Option<ObjectId>> {
        Self::tip(repo, &format!("refs/heads/{}", Self::branch(repo)?))
    }

    fn tracking_ref(repo: &Repository) -> Result<String> {
        let branch = Self::branch(repo)?;
        if Self::config_value(repo, &format!("branch.{branch}.remote")).as_deref() != Some("origin") {
            return Err(git("当前分支必须跟踪 origin 工作日志仓库"));
        }
        let name = Self::config_value(repo, &format!("branch.{branch}.merge"))
            .ok_or_else(|| git("缺少远端跟踪分支"))?;
        Self::validate_branch(&name)?;
        Ok(name)
    }

    fn validate_branch(name: &str) -> Result<()> {
        if !name.starts_with("refs/heads/") { return Err(git("无效的远端分支")); }
        let _: gix::refs::FullName = name.try_into().map_err(git)?;
        Ok(())
    }

    fn remote_ref(merge: &str) -> String {
        format!("refs/remotes/origin/{}", merge.trim_start_matches("refs/heads/"))
    }

    fn validate_origin(repo: &Repository, expected: &str) -> Result<()> {
        let expected = repository_id(&Self::normalize_url(expected)?);
        let config = repo.config_snapshot();
        for key in ["remote.origin.url", "remote.origin.pushurl"] {
            match config.plumbing().raw_values(key) {
                Ok(values) if !values.is_empty() => {
                    for value in values {
                        let value = value.to_str().map_err(git)?;
                        if value.is_empty() || repository_id(&Self::normalize_url(value)?) != expected {
                            return Err(git("origin 与配置的工作日志仓库不一致"));
                        }
                    }
                }
                Err(_) if key.ends_with(".pushurl") => {},
                _ => return Err(git("origin 没有配置仓库 URL")),
            }
        }
        Ok(())
    }

    // protocol::Error 为 gix 外部类型，Err 体积无法缩小，接受该 lint。
    #[allow(clippy::result_large_err)]
    fn credentials(pat: Option<String>) -> impl FnMut(gix::credentials::helper::Action) -> gix::credentials::protocol::Result {
        move |action| {
            use gix::credentials::{helper::Action, protocol::Outcome};
            match (action, pat.as_ref()) {
                (Action::Get(mut context), Some(pat)) => {
                    if pat.bytes().any(|b| b.is_ascii_control()) {
                        return Err(gix::credentials::protocol::Error::Quit);
                    }
                    context.username = Some("pat".into());
                    context.password = Some(pat.clone());
                    Ok(Some(Outcome {
                        identity: gix::sec::identity::Account {
                            username: "pat".into(), password: pat.clone(), oauth_refresh_token: None,
                        },
                        next: context.into(),
                    }))
                }
                _ => Ok(None), // Never run credential helpers, prompt, or persist the PAT.
            }
        }
    }

    fn fetch(&self, repo: &Repository) -> Result<Option<String>> {
        // gix updates fetch reflogs using the repository's cached committer.
        // Fill missing identity, then reopen so its cache sees the persisted values.
        Self::signature(repo)?;
        let reopened = Self::open(repo.workdir().ok_or_else(|| git("缺少工作区"))?)?;
        let repo = &reopened;
        let url = Self::config_value(repo, "remote.origin.url").ok_or_else(|| git("缺少 origin"))?;
        if let Some(path) = Self::local_path(&url)? {
            let remote = gix::open(path).map_err(git)?;
            let head = format!("refs/heads/{}", Self::branch(&remote)?);
            let references = remote.references().map_err(git)?;
            for reference in references.local_branches().map_err(git)? {
                let mut reference = reference.map_err(git)?;
                let name = reference.name().as_bstr().to_str().map_err(git)?.to_owned();
                let id = reference.peel_to_id().map_err(git)?.detach();
                Self::copy_objects(&remote, repo, id)?;
                Self::update_ref(repo, &Self::remote_ref(&name), id, PreviousValue::Any, "sync: fetch")?;
            }
            return Ok(Some(head));
        }
        Self::network_url(&url)?;
        let pat = self.pat();
        let result = (|| {
            let remote = repo.find_remote("origin").map_err(git)?;
            // Use gix's protocol with our Http implementation. The stock reqwest
            // adapter constructs a platform-root client internally in reqwest 0.13.
            let http = RustlsHttp(Self::client()?);
            let transport = http::Transport::new_http(http, gix::url::parse(url.as_bytes().as_bstr()).map_err(git)?, transport::Protocol::V1, false);
            let prepared = remote.to_connection_with_transport(transport)
                .with_credentials(Self::credentials(pat.clone()))
                .prepare_fetch(gix::progress::Discard, Default::default()).map_err(git)?;
            let advertised = prepared.ref_map().remote_refs.iter().find_map(|reference| {
                use gix::protocol::handshake::Ref;
                match reference {
                    Ref::Symbolic { full_ref_name, target, .. } if full_ref_name.as_bstr() == b"HEAD".as_bstr() => Some(target.to_string()),
                    Ref::Unborn { full_ref_name, target } if full_ref_name.as_bstr() == b"HEAD".as_bstr() => Some(target.to_string()),
                    _ => None,
                }
            });
            prepared.receive(gix::progress::Discard, &AtomicBool::new(false)).map_err(git)?;
            Ok(advertised)
        })();
        result.map_err(|error: SyncError| git(safe_message(&error.to_string(), pat.as_deref())))
    }

    fn converge(&self, repo_dir: &Path, expected: &str) -> Result<()> {
        let repo = Self::open(repo_dir)?;
        if Self::config_value(&repo, "remote.origin.url").is_none() {
            Self::configure(&repo, &[("remote.origin.url", &Self::normalize_url(expected)?)])?;
        }
        let repo = Self::open(repo_dir)?;
        Self::validate_origin(&repo, expected)?;
        Self::protect(&repo)?;
        let mut branch = Self::branch(&repo)?;
        let mut remote = Self::config_value(&repo, &format!("branch.{branch}.remote"));
        let mut merge = Self::config_value(&repo, &format!("branch.{branch}.merge"));
        if remote.as_deref().is_some_and(|r| r != "origin") { return Err(git("当前分支必须跟踪 origin")); }
        if let Some(name) = &merge { Self::validate_branch(name)?; }
        Self::configure(&repo, &[("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")])?;
        if remote.is_none() || merge.is_none() {
            let repo = Self::open(repo_dir)?;
            let unborn = Self::head(&repo)?.is_none();
            let advertised = self.fetch(&repo)?;
            if unborn && remote.is_none() && merge.is_none() {
                let head = advertised.as_deref().unwrap_or(MAIN);
                Self::validate_branch(head)?;
                Self::set_head(&repo, head)?;
                branch = head.trim_start_matches("refs/heads/").to_owned();
            }
            remote.get_or_insert_with(|| "origin".into());
            merge.get_or_insert_with(|| format!("refs/heads/{branch}"));
            if !unborn && Self::tip(&repo, &Self::remote_ref(merge.as_deref().unwrap()))?.is_none() {
                return Err(git("无法自愈：对应的远端跟踪分支不存在"));
            }
            Self::configure(&repo, &[
                (&format!("branch.{branch}.remote"), remote.as_deref().unwrap()),
                (&format!("branch.{branch}.merge"), merge.as_deref().unwrap()),
            ])?;
            let repo = Self::open(repo_dir)?;
            if unborn {
                if let Some(target) = Self::tip(&repo, &Self::remote_ref(merge.as_deref().unwrap()))? {
                    Self::checkout(&repo, target)?;
                }
            }
        }
        Self::tracking_ref(&Self::open(repo_dir)?)?;
        Ok(())
    }

    fn identity(repo: &Repository) -> gix::actor::Signature {
        let name = Self::config_value(repo, "user.name").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "Sticky Todo".into());
        let email = Self::config_value(repo, "user.email").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "sticky-todo@localhost".into());
        gix::actor::Signature { name: name.into(), email: email.into(), time: gix::date::Time::now_utc() }
    }

    fn signature(repo: &Repository) -> Result<gix::actor::Signature> {
        let signature = Self::identity(repo);
        let mut changes = Vec::new();
        for (key, value) in [("user.name", signature.name.to_str().map_err(git)?),
            ("user.email", signature.email.to_str().map_err(git)?)] {
            if Self::config_value(repo, key).is_none_or(|s| s.trim().is_empty()) { changes.push((key, value)); }
        }
        if !changes.is_empty() { Self::configure(repo, &changes)?; }
        Ok(signature)
    }

    fn path(root: &Path, relative: &str) -> Result<PathBuf> {
        let path = Path::new(relative);
        if relative.contains('\\') || !path.components().all(|c| matches!(c, Component::Normal(_)))
            || path.components().any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")) {
            return Err(git("仓库包含不安全的工作区路径"));
        }
        let mut current = root.to_owned();
        for part in path.components() {
            current.push(part);
            if let Ok(meta) = fs::symlink_metadata(&current) {
                if meta.file_type().is_symlink() { return Err(git("工作日志不支持符号链接")); }
            }
        }
        Ok(current)
    }

    fn files_from_index(repo: &Repository, index: &gix::index::File) -> Result<Files> {
        let mut files = Files::new();
        for entry in index.entries() {
            let kind = match entry.mode {
                gix::index::entry::Mode::FILE => gix::objs::tree::EntryKind::Blob,
                gix::index::entry::Mode::FILE_EXECUTABLE => gix::objs::tree::EntryKind::BlobExecutable,
                _ => return Err(git("工作日志不支持符号链接、子模块或未合并索引")),
            };
            if entry.stage_raw() != 0 { return Err(git("索引存在未解决的冲突")); }
            let path = entry.path(index).to_str().map_err(git)?.to_owned();
            Self::path(repo.workdir().unwrap_or(repo.git_dir()), &path)?;
            files.insert(path, (kind, repo.find_blob(entry.id).map_err(git)?.data.to_vec()));
        }
        Ok(files)
    }

    fn head_files(repo: &Repository) -> Result<Files> {
        let tree = repo.head_tree_id_or_empty().map_err(git)?;
        Self::files_from_index(repo, &repo.index_from_tree(tree.as_ref()).map_err(git)?)
    }

    fn worktree_files(repo: &Repository) -> Result<Files> {
        let root = repo.workdir().ok_or_else(|| git("缺少工作区"))?;
        let index = repo.index_or_empty().map_err(git)?;
        let mut paths: BTreeSet<String> = index.entries().iter().map(|e| e.path(&index).to_str().map(str::to_owned))
            .collect::<std::result::Result<_, _>>().map_err(git)?;
        // gix supplies ignore-aware untracked discovery. Read/store blobs raw:
        // checkout/commit never invoke CRLF or .gitattributes clean/smudge filters.
        for item in repo.status(gix::progress::Discard).map_err(git)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .into_iter(Vec::<BString>::new()).map_err(git)? {
            paths.insert(item.map_err(git)?.location().to_str().map_err(git)?.to_owned());
        }
        let mut files = Files::new();
        for relative in paths {
            let path = Self::path(root, &relative)?;
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => continue,
                Err(e) => return Err(git(e)),
            };
            if meta.is_dir() { continue; }
            if !meta.is_file() { return Err(git("工作日志仅支持普通文件")); }
            let mut kind = gix::objs::tree::EntryKind::Blob;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 { kind = gix::objs::tree::EntryKind::BlobExecutable; }
            }
            #[cfg(not(unix))]
            if index.entries().iter().any(|e| e.path(&index) == relative.as_bytes().as_bstr()
                && e.mode == gix::index::entry::Mode::FILE_EXECUTABLE) { kind = gix::objs::tree::EntryKind::BlobExecutable; }
            files.insert(relative, (kind, fs::read(path).map_err(git)?));
        }
        Ok(files)
    }

    fn dirty(repo: &Repository) -> Result<bool> {
        let head = Self::head_files(repo)?;
        let index = repo.index_or_empty().map_err(git)?;
        Ok(Self::files_from_index(repo, &index)? != head
            || Self::worktree_files(repo)? != head)
    }

    fn require_clean(repo: &Repository) -> Result<()> {
        for marker in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply", "index.lock"] {
            if repo.git_dir().join(marker).exists() { return Err(git("工作区存在进行中的 Git 操作")); }
        }
        if Self::dirty(repo)? { return Err(git("pull 需要干净工作区；未提交内容已保留")); }
        Ok(())
    }

    fn write_tree(repo: &Repository, files: &Files) -> Result<ObjectId> {
        let mut tree = repo.edit_tree(ObjectId::empty_tree(repo.object_hash())).map_err(git)?;
        for (path, (kind, bytes)) in files {
            let id = repo.write_blob(bytes).map_err(git)?;
            tree.upsert(path.as_str(), *kind, id).map_err(git)?;
        }
        tree.write().map(|id| id.detach()).map_err(git)
    }

    fn ancestors(repo: &Repository, tip: ObjectId) -> Result<HashSet<ObjectId>> {
        repo.rev_walk([tip]).all().map_err(git)?.map(|item| item.map(|c| c.id).map_err(git)).collect()
    }

    fn checkout(repo: &Repository, target: ObjectId) -> Result<()> {
        Self::require_clean(repo)?;
        let root = repo.workdir().ok_or_else(|| git("缺少工作区"))?;
        let old = Self::head_files(repo)?;
        let tree = repo.find_commit(target).map_err(git)?.tree_id().map_err(git)?.detach();
        let next = Self::files_from_index(repo, &repo.index_from_tree(&tree).map_err(git)?)?;
        // Preflight ignored/untracked collisions too; status alone would miss them.
        for relative in next.keys() {
            let path = Self::path(root, relative)?;
            if path.exists() && !old.contains_key(relative) {
                return Err(git("checkout 会覆盖未跟踪内容；已取消"));
            }
            let mut parent = path.parent();
            while let Some(p) = parent.filter(|p| *p != root) {
                if p.is_file() { return Err(git("checkout 路径被文件占用；已取消")); }
                parent = p.parent();
            }
        }
        let previous = Self::head(repo)?;
        let index_before = match fs::read(repo.index_path()) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(git(e)),
        };
        let result = (|| {
            Self::install_files(root, &old, &next)?;
            repo.index_from_tree(&tree).map_err(git)?.write(Default::default()).map_err(git)?;
            let name = format!("refs/heads/{}", Self::branch(repo)?);
            Self::update_ref(repo, &name, target, expected(previous), "sync: checkout")?;
            Ok(())
        })();
        if let Err(error) = result {
            // Ref is updated last and atomically. Restore both files and index
            // on an I/O/ref-lock failure; report rollback failure explicitly.
            let rollback = (|| {
                Self::install_files(root, &next, &old)?;
                match index_before {
                    Some(bytes) => atomic_write(&repo.index_path(), &bytes)?,
                    None => { if repo.index_path().exists() { fs::remove_file(repo.index_path()).map_err(git)?; } },
                }
                Ok::<_, SyncError>(())
            })();
            return match rollback {
                Ok(()) => Err(error),
                Err(restore) => Err(git(format!("{error}; 恢复工作区失败: {restore}"))),
            };
        }
        Ok(())
    }

    fn install_files(root: &Path, old: &Files, next: &Files) -> Result<()> {
        for relative in old.keys().filter(|p| !next.contains_key(*p)) {
            let path = Self::path(root, relative)?;
            match fs::remove_file(&path) {
                Ok(()) => {}, Err(e) if e.kind() == io::ErrorKind::NotFound => {}, Err(e) => return Err(git(e)),
            }
            let mut parent = path.parent();
            while let Some(p) = parent.filter(|p| *p != root) {
                if fs::remove_dir(p).is_err() { break; } // Empty directories only.
                parent = p.parent();
            }
        }
        for (relative, (kind, bytes)) in next {
            if old.get(relative) == next.get(relative) { continue; }
            let path = Self::path(root, relative)?;
            fs::create_dir_all(path.parent().ok_or_else(|| git("无效文件路径"))?).map_err(git)?;
            fs::write(&path, bytes).map_err(git)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if *kind == gix::objs::tree::EntryKind::BlobExecutable { 0o755 } else { 0o644 };
                fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(git)?;
            }
            #[cfg(not(unix))]
            let _ = kind;
        }
        Ok(())
    }

    fn objects(repo: &Repository, tip: ObjectId) -> Result<Vec<ObjectId>> {
        let mut pending = vec![tip];
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) { continue; }
            let object = repo.find_object(id).map_err(git)?;
            match object.kind {
                gix::objs::Kind::Commit => {
                    let commit = repo.find_commit(id).map_err(git)?;
                    pending.push(commit.tree_id().map_err(git)?.detach());
                    pending.extend(commit.parent_ids().map(|id| id.detach()));
                }
                gix::objs::Kind::Tree => {
                    let tree = gix::objs::TreeRef::from_bytes(&object.data, repo.object_hash()).map_err(git)?;
                    for entry in tree.entries {
                        if !entry.mode.is_commit() { pending.push(entry.oid.to_owned()); }
                    }
                }
                gix::objs::Kind::Blob => {},
                _ => return Err(git("提交可达图包含不支持的对象类型")),
            }
            result.push(id);
        }
        Ok(result)
    }

    fn copy_objects(source: &Repository, destination: &Repository, tip: ObjectId) -> Result<()> {
        use gix::objs::Write;
        for id in Self::objects(source, tip)? {
            let object = source.find_object(id).map_err(git)?;
            let written = destination.write_buf(object.kind, &object.data).map_err(git)?;
            if written != id { return Err(git("对象复制校验失败")); }
        }
        Ok(())
    }

    fn full_pack(repo: &Repository, tip: ObjectId) -> Result<Vec<u8>> {
        use gix_pack::data::{output, Version};
        let objects = Self::objects(repo, tip)?;
        let count = u32::try_from(objects.len()).map_err(git)?;
        let mut entries = Vec::with_capacity(objects.len());
        for id in objects {
            let object = repo.find_object(id).map_err(git)?;
            entries.push(output::Entry::from_data(&output::Count::from_data(id, None),
                &gix::objs::Data { kind: object.kind, data: &object.data, object_hash: repo.object_hash() }, Default::default()).map_err(git)?);
        }
        let input = std::iter::once(Ok::<_, io::Error>(entries));
        let mut output = output::bytes::FromEntriesIter::new(input, Vec::new(), count, Version::V2, repo.object_hash());
        for written in output.by_ref() { written.map_err(git)?; }
        if output.digest().is_none() { return Err(git("pack 校验和未完成")); }
        Ok(output.into_write())
    }

    fn push_http(&self, repo: &Repository, url: &str, branch: &str, tip: ObjectId) -> Result<()> {
        // Production journal uses main; honour validated branch.* for old clones
        // and the migrated non-main tracking regression, always one ref per push.
        Self::validate_branch(branch)?;
        Self::network_url(url)?;
        let client = Self::client()?;
        let pat = self.pat();
        let result = (|| {
            let base = url.trim_end_matches('/');
            let auth = |r: reqwest::blocking::RequestBuilder| match &pat {
                Some(pat) => r.basic_auth("pat", Some(pat)), None => r,
            };
            let advertisement = response_bytes(auth(client.get(format!("{base}/info/refs?service=git-receive-pack")))
                .header("Accept", "application/x-git-receive-pack-advertisement")
                .send().map_err(|e| git(e.without_url()))?, "application/x-git-receive-pack-advertisement")?;
            let old = advertised_oid(&advertisement, branch)?;
            if old == tip { return Ok(()); }
            // A valid old-oid alone does NOT prevent force pushes on permissive
            // servers. Explicit ancestry check is essential; unknown means fetch.
            if !old.is_null() && !Self::ancestors(repo, tip)?.contains(&old) {
                return Err(git("push rejected: non-fast-forward; fetch first"));
            }
            let body = push_body(old, tip, branch, &Self::full_pack(repo, tip)?)?;
            let response = response_bytes(auth(client.post(format!("{base}/git-receive-pack")))
                .header("Content-Type", "application/x-git-receive-pack-request")
                .header("Accept", "application/x-git-receive-pack-result")
                .body(body).send().map_err(|e| git(e.without_url()))?, "application/x-git-receive-pack-result")?;
            report_status(&response, branch)
        })();
        result.map_err(|e: SyncError| git(safe_message(&e.to_string(), pat.as_deref())))
    }

    fn do_push(&self, repo_dir: &Path) -> Result<()> {
        let repo = Self::open(repo_dir)?;
        let branch = Self::tracking_ref(&repo)?;
        let tip = Self::head(&repo)?.ok_or_else(|| git("没有可推送提交"))?;
        let url = Self::config_value(&repo, "remote.origin.pushurl")
            .or_else(|| Self::config_value(&repo, "remote.origin.url")).ok_or_else(|| git("缺少 origin"))?;
        if let Some(path) = Self::local_path(&url)? {
            let remote = gix::open(path).map_err(git)?;
            if !remote.is_bare() { return Err(git("本地推送目标必须是 bare 仓库")); }
            let old = Self::tip(&remote, &branch)?;
            if old != Some(tip) {
                if let Some(old) = old {
                    if !Self::ancestors(&repo, tip)?.contains(&old) {
                        return Err(git("push rejected: non-fast-forward; fetch first"));
                    }
                }
                Self::copy_objects(&repo, &remote, tip)?;
                Self::update_ref(&remote, &branch, tip, expected(old), "sync: receive-pack")
                    .map_err(|e| git(format!("push rejected: {e}")))?;
            }
        } else {
            self.push_http(&repo, &url, &branch, tip)?;
        }
        Self::update_ref(&repo, &Self::remote_ref(&branch), tip, PreviousValue::Any, "sync: push")?;
        Ok(())
    }
}

impl GitBackend for GixBackend {
    fn ensure_cloned(&self, repo_dir: &Path, url: &str) -> Result<bool> {
        self.operation("ensure_cloned", || {
            if repo_dir.join(".git").exists() { self.converge(repo_dir, url)?; return Ok(false); }
            if repo_dir.exists() && fs::read_dir(repo_dir).map_err(git)?.next().transpose().map_err(git)?.is_some() {
                return Err(git("目录存在但不是空目录或 Git 仓库"));
            }
            let url = Self::normalize_url(url)?;
            if Self::local_path(&url)?.is_none() { Self::network_url(&url)?; }
            // 统一自建克隆：gix::init + converge（converge 的 fetch 对本地路径与 https
            // 均为自研实现）。PrepareFetch 的 persist 在真机上抛 "Could not open data"，
            // 且网络克隆并无其预设之外的收益。
            let repo = gix::init(repo_dir).map_err(git)?;
            Self::set_head(&repo, MAIN)?;
            Self::configure(&repo, &[("remote.origin.url", &url)])?;
            drop(repo);
            self.converge(repo_dir, &url)?;
            Ok(true)
        })
    }

    fn pull_rebase(&self, repo_dir: &Path) -> Result<()> {
        self.operation("fetch/rebase", || {
            let repo = Self::open(repo_dir)?;
            Self::require_clean(&repo)?;
            let branch = Self::tracking_ref(&repo)?;
            self.fetch(&repo)?;
            let upstream = Self::tip(&repo, &Self::remote_ref(&branch))?.ok_or_else(|| git("远端跟踪分支不存在"))?;
            let Some(local) = Self::head(&repo)? else { return Self::checkout(&repo, upstream); };
            if Self::ancestors(&repo, local)?.contains(&upstream) { return Ok(()); }
            if Self::ancestors(&repo, upstream)?.contains(&local) { return Self::checkout(&repo, upstream); }
            let base = repo.merge_base(local, upstream).map_err(git)?;
            let tree_of = |id| repo.find_commit(id).map_err(git)?.tree_id().map(|id| id.detach()).map_err(git);
            let mut merged = repo.merge_trees(tree_of(base.detach())?, tree_of(local)?, tree_of(upstream)?,
                Default::default(), repo.tree_merge_options().map_err(git)?).map_err(git)?;
            if merged.has_unresolved_conflicts(Default::default()) {
                return Err(SyncError::Conflict("pull 三方合并冲突；本地提交和工作区未变更".into()));
            }
            let tree = merged.tree.write().map_err(git)?.detach();
            let signature = Self::signature(&repo)?;
            let replay = gix::objs::Commit {
                tree, parents: [upstream].into_iter().collect(), author: signature.clone(), committer: signature,
                encoding: None,
                message: repo.find_commit(local).map_err(git)?.message_raw().map_err(git)?.to_owned(),
                extra_headers: Vec::new(),
            };
            let replay = repo.write_object(&replay).map_err(git)?.detach();
            Self::checkout(&repo, replay)
        })
    }

    fn commit_all(&self, repo_dir: &Path, message: &str) -> Result<bool> {
        self.operation("commit", || {
            let repo = Self::open(repo_dir)?;
            if !Self::dirty(&repo)? { return Ok(false); }
            let signature = Self::signature(&repo)?;
            let files = Self::worktree_files(&repo)?;
            let tree = Self::write_tree(&repo, &files)?;
            // An all-file index built from the raw snapshot is equivalent to add -A,
            // including deletions, tracked ignored files and newly discovered files.
            repo.index_from_tree(&tree).map_err(git)?.write(Default::default()).map_err(git)?;
            if tree == repo.head_tree_id_or_empty().map_err(git)?.detach() { return Ok(false); }
            let parents: Vec<_> = Self::head(&repo)?.into_iter().collect();
            let mut time = Default::default();
            let signature = signature.to_ref(&mut time);
            repo.commit_as(signature, signature, "HEAD", message, tree, parents).map_err(git)?;
            Ok(true)
        })
    }

    fn push(&self, repo_dir: &Path) -> Result<()> {
        self.operation("push", || self.do_push(repo_dir))
    }

    fn push_with_retry(&self, repo_dir: &Path) -> Result<bool> {
        match self.push(repo_dir) {
            Ok(()) => Ok(false),
            Err(SyncError::Git(detail)) if is_push_rejected(&detail) => {
                self.pull_rebase(repo_dir)?;
                self.push(repo_dir)?;
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    fn unpushed_count(&self, repo_dir: &Path) -> Result<usize> {
        let repo = Self::open(repo_dir)?;
        let upstream = Self::tip(&repo, &Self::remote_ref(&Self::tracking_ref(&repo)?))?
            .ok_or_else(|| git("远端跟踪分支不存在"))?;
        let head = Self::head(&repo)?.ok_or_else(|| git("HEAD 尚无提交"))?;
        let remote = Self::ancestors(&repo, upstream)?;
        Ok(Self::ancestors(&repo, head)?.difference(&remote).count())
    }

    fn has_local_changes(&self, repo_dir: &Path) -> bool {
        Self::open(repo_dir).and_then(|repo| Self::dirty(&repo)).unwrap_or(true)
    }
}

fn expected(previous: Option<ObjectId>) -> PreviousValue {
    previous.map(|id| PreviousValue::MustExistAndMatch(id.into())).unwrap_or(PreviousValue::MustNotExist)
}

struct FileLock { path: PathBuf, target: PathBuf, file: Option<fs::File>, committed: bool }
impl FileLock {
    fn new(target: &Path) -> Result<Self> {
        let mut path = target.as_os_str().to_owned(); path.push(".lock");
        let path = PathBuf::from(path);
        let file = fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(git)?;
        Ok(Self { path, target: target.to_owned(), file: Some(file), committed: false })
    }
    fn commit(mut self) -> Result<()> {
        self.file.take().expect("live lock").sync_all().map_err(git)?;
        fs::rename(&self.path, &self.target).map_err(git)?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.committed { let _ = fs::remove_file(&self.path); }
    }
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut lock = FileLock::new(path)?;
    lock.file.as_mut().expect("live lock").write_all(bytes).map_err(git)?;
    lock.commit()
}

// pkt-line's four ASCII hexadecimal length bytes are framing, not a "v4"
// Git protocol. receive-pack uses protocol v0/v1 and a pack version 2 payload.
fn packet(data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let length = data.len() + 4;
    if length > 65520 { return Err(git("pkt-line 太长")); }
    out.extend_from_slice(format!("{length:04x}").as_bytes());
    out.extend_from_slice(data);
    Ok(())
}

fn packets(mut data: &[u8]) -> Result<Vec<Option<&[u8]>>> {
    let mut lines = Vec::new();
    while !data.is_empty() {
        if data.len() < 4 { return Err(git("pkt-line 长度截断")); }
        if !data[..4].iter().all(u8::is_ascii_hexdigit) { return Err(git("pkt-line 长度不是十六进制")); }
        let size = usize::from_str_radix(std::str::from_utf8(&data[..4]).map_err(git)?, 16).map_err(git)?;
        if size == 0 { lines.push(None); data = &data[4..]; continue; }
        if !(4..=65520).contains(&size) || size > data.len() { return Err(git("无效或截断的 pkt-line")); }
        lines.push(Some(&data[4..size])); data = &data[size..];
    }
    Ok(lines)
}

fn advertised_oid(data: &[u8], branch: &str) -> Result<ObjectId> {
    let lines = packets(data)?;
    if lines.first().copied().flatten() != Some(b"# service=git-receive-pack\n".as_slice())
        || lines.get(1) != Some(&None) || lines.last() != Some(&None) {
        return Err(git("无效的 receive-pack advertisement"));
    }
    let mut old = None;
    let mut supports_status = false;
    let mut count = 0;
    for line in lines.iter().skip(2).flatten() {
        if line.starts_with(b"ERR ") { return Err(git(String::from_utf8_lossy(line))); }
        let mut parts = line.splitn(2, |b| *b == 0);
        let reference = std::str::from_utf8(parts.next().unwrap()).map_err(git)?.trim_end_matches('\n');
        if let Some(capabilities) = parts.next() {
            supports_status |= std::str::from_utf8(capabilities).map_err(git)?.split_ascii_whitespace().any(|c| c == "report-status");
        }
        let (oid, name) = reference.split_once(' ').ok_or_else(|| git("无效的远端引用"))?;
        let oid = ObjectId::from_hex(oid.as_bytes()).map_err(git)?;
        if name == branch && old.replace(oid).is_some() {
            return Err(git("重复的远端分支"));
        }
        count += 1;
    }
    if !supports_status || count == 0 { return Err(git("远端不支持 report-status")); }
    Ok(old.unwrap_or_else(|| ObjectId::null(gix::hash::Kind::Sha1)))
}

fn push_body(old: ObjectId, new: ObjectId, branch: &str, pack: &[u8]) -> Result<Vec<u8>> {
    GixBackend::validate_branch(branch)?;
    let mut body = Vec::new();
    packet(format!("{old} {new} {branch}\0report-status\n").as_bytes(), &mut body)?;
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    Ok(body)
}

fn report_status(data: &[u8], branch: &str) -> Result<()> {
    let lines = packets(data).map_err(|e| git(format!("push rejected: {e}")))?;
    if lines.len() != 3 || lines.last() != Some(&None) {
        let details = lines.iter().flatten().map(|line| String::from_utf8_lossy(line).trim().to_owned()).collect::<Vec<_>>().join("; ");
        return Err(git(format!("push rejected: 不完整的 report-status; {details}")));
    }
    let unpack = lines[0].ok_or_else(|| git("push rejected: 缺少 unpack 状态"))?;
    let status = lines[1].ok_or_else(|| git("push rejected: 缺少分支状态"))?;
    if unpack.strip_suffix(b"\n").unwrap_or(unpack) != b"unpack ok"
        || status.strip_suffix(b"\n").unwrap_or(status) != format!("ok {branch}").as_bytes() {
        return Err(git(format!("push rejected: {}; {}", String::from_utf8_lossy(unpack).trim(), String::from_utf8_lossy(status).trim())));
    }
    Ok(())
}

fn response_bytes(mut response: reqwest::blocking::Response, content_type: &str) -> Result<Vec<u8>> {
    let status = response.status();
    let actual_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|h| h.to_str().ok()).unwrap_or("").to_owned();
    let mut bytes = Vec::new();
    response.by_ref().take(MAX_HTTP_BYTES + 1).read_to_end(&mut bytes).map_err(git)?;
    if bytes.len() as u64 > MAX_HTTP_BYTES { return Err(git("HTTP 响应超出小仓库限制")); }
    if !status.is_success() {
        return Err(git(format!("HTTP {}: {}", status.as_u16(), String::from_utf8_lossy(&bytes))));
    }
    if actual_type.split(';').next().map(str::trim) != Some(content_type) { return Err(git("HTTP 响应不是 Git smart protocol")); }
    Ok(bytes)
}

// gix Http adapter with a caller-owned TLS client. POST is deferred until the
// protocol has finished writing its body and begins reading response headers.
// Buffering is deliberate for the small journal and avoids a worker/runtime per
// request. Both header/body readers observe the same single request/result.
struct RustlsHttp(reqwest::blocking::Client);
type HttpOutcome = std::result::Result<(Vec<u8>, Vec<u8>), (io::ErrorKind, String)>;

struct HttpExchange {
    request: Option<reqwest::blocking::RequestBuilder>,
    upload: Vec<u8>,
    response: Option<HttpOutcome>,
}
struct HttpReader { shared: Arc<Mutex<HttpExchange>>, headers: bool, offset: usize }
struct HttpWriter(Arc<Mutex<HttpExchange>>);

impl Read for HttpReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() { return Ok(0); }
        let mut state = self.shared.lock().map_err(|_| io::Error::other("HTTP 状态锁失败"))?;
        if state.response.is_none() {
            let request = state.request.take().ok_or_else(|| io::Error::other("HTTP 请求已消耗"))?;
            let upload = std::mem::take(&mut state.upload);
            state.response = Some((|| {
                let mut response = request.body(upload).send().map_err(|e| (io::ErrorKind::Other, e.without_url().to_string()))?;
                if !response.status().is_success() {
                    let kind = if response.status().as_u16() == 401 { io::ErrorKind::PermissionDenied } else { io::ErrorKind::Other };
                    return Err((kind, format!("HTTP {}", response.status().as_u16())));
                }
                let mut headers = Vec::new();
                for (key, value) in response.headers() {
                    headers.extend_from_slice(key.as_str().as_bytes()); headers.extend_from_slice(b": ");
                    headers.extend_from_slice(value.as_bytes()); headers.push(b'\n');
                }
                let mut body = Vec::new();
                response.by_ref().take(MAX_HTTP_BYTES + 1).read_to_end(&mut body).map_err(|e| (e.kind(), e.to_string()))?;
                if body.len() as u64 > MAX_HTTP_BYTES { return Err((io::ErrorKind::Other, "HTTP 响应超出小仓库限制".into())); }
                Ok((headers, body))
            })());
        }
        match state.response.as_ref().expect("response initialized") {
            Err((kind, message)) => Err(io::Error::new(*kind, message.clone())),
            Ok((headers, body)) => {
                let bytes = if self.headers { headers } else { body };
                let count = out.len().min(bytes.len().saturating_sub(self.offset));
                out[..count].copy_from_slice(&bytes[self.offset..self.offset + count]); self.offset += count;
                Ok(count)
            }
        }
    }
}
impl Write for HttpWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.0.lock().map_err(|_| io::Error::other("HTTP 状态锁失败"))?;
        if state.upload.len() + bytes.len() > MAX_HTTP_BYTES as usize { return Err(io::Error::other("HTTP 请求超出小仓库限制")); }
        state.upload.extend_from_slice(bytes); Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
impl RustlsHttp {
    fn exchange(&self, mut request: reqwest::blocking::RequestBuilder, headers: impl IntoIterator<Item = impl AsRef<str>>)
        -> http::PostResponse<BufReader<HttpReader>, BufReader<HttpReader>, HttpWriter> {
        for header in headers {
            if let Some((name, value)) = header.as_ref().split_once(':') { request = request.header(name, value.trim()); }
        }
        let shared = Arc::new(Mutex::new(HttpExchange { request: Some(request), upload: Vec::new(), response: None }));
        http::PostResponse {
            headers: BufReader::new(HttpReader { shared: shared.clone(), headers: true, offset: 0 }),
            body: BufReader::new(HttpReader { shared: shared.clone(), headers: false, offset: 0 }),
            post_body: HttpWriter(shared),
        }
    }
}
impl http::Http for RustlsHttp {
    type Headers = BufReader<HttpReader>;
    type ResponseBody = BufReader<HttpReader>;
    type PostBody = HttpWriter;
    fn get(&mut self, url: &str, _base: &str, headers: impl IntoIterator<Item = impl AsRef<str>>)
        -> std::result::Result<http::GetResponse<Self::Headers, Self::ResponseBody>, http::Error> {
        Ok(self.exchange(self.0.get(url), headers).into())
    }
    fn post(&mut self, url: &str, _base: &str, headers: impl IntoIterator<Item = impl AsRef<str>>, _kind: http::PostBodyDataKind)
        -> std::result::Result<http::PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>, http::Error> {
        Ok(self.exchange(self.0.post(url), headers))
    }
    fn configure(&mut self, _config: &dyn std::any::Any) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Trust is deliberately independent of Git's http.sslCAInfo/sslVerify.
        Ok(())
    }
}

#[cfg(test)]
#[path = "sync_gix_test.rs"]
mod tests;
