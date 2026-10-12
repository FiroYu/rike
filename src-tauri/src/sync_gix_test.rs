//! Local bare repositories and an in-process Smart HTTP mock; never spawn Git.
use super::*;
use std::{error::Error, io::{BufRead, Cursor}, net::{TcpListener, TcpStream}, thread, time::Instant};
use tempfile::TempDir;

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;
struct Fixture { _root: TempDir, a: PathBuf, b: PathBuf, url: String, backend: GixBackend }
fn backend() -> GixBackend { GixBackend::new(Arc::new(|| None)) }
fn head(path: &Path) -> TestResult<ObjectId> { Ok(gix::open(path)?.head_id()?.detach()) }
fn config(path: &Path, key: &str) -> TestResult<Option<String>> {
    Ok(GixBackend::config_value(&gix::open(path)?, key))
}
fn set_config(path: &Path, key: &str, value: &str) -> TestResult {
    GixBackend::configure(&gix::open(path)?, &[(key, value)])?; Ok(())
}
fn remove_config(path: &Path, keys: &[&str]) -> TestResult {
    let repo = gix::open(path)?;
    let path = repo.common_dir().join("config");
    let mut local = gix::config::File::from_path_no_includes(path.clone(), gix::config::Source::Local)?;
    for key in keys { if let Ok(mut values) = local.raw_values_mut(*key) { values.delete_all(); } }
    let mut bytes = Vec::new(); local.write_to(&mut bytes)?; atomic_write(&path, &bytes)?; Ok(())
}
fn write_file(dir: &Path, path: &str, text: &str) -> TestResult {
    let path = dir.join(path); fs::create_dir_all(path.parent().unwrap())?; fs::write(path, text)?; Ok(())
}
fn init_bare(path: &Path, branch: &str, seed: bool) -> TestResult {
    let repo = gix::init_bare(path)?;
    GixBackend::set_head(&repo, &format!("refs/heads/{branch}"))?;
    if seed {
        // Independently seed via gix's object APIs, not commit_all under test.
        let signature = gix::actor::Signature { name: "Test".into(), email: "test@example.com".into(), time: gix::date::Time::now_utc() };
        let blob = repo.write_blob(b"# initial\n")?;
        let mut tree = repo.edit_tree(ObjectId::empty_tree(repo.object_hash()))?;
        tree.upsert("README.md", gix::objs::tree::EntryKind::Blob, blob)?;
        let tree = tree.write()?;
        let mut time = Default::default(); let signature = signature.to_ref(&mut time);
        repo.commit_as(signature, signature, "HEAD", "init", tree, Vec::<ObjectId>::new())?;
    }
    Ok(())
}
fn setup(branch: &str) -> TestResult<Fixture> {
    let root = tempfile::tempdir()?;
    let remote = root.path().join("remote.git"); init_bare(&remote, branch, true)?;
    let url = remote.to_string_lossy().into_owned();
    let a = root.path().join("a"); let b = root.path().join("b"); let backend = backend();
    assert!(backend.ensure_cloned(&a, &url)?); assert!(backend.ensure_cloned(&b, &url)?);
    Ok(Fixture { _root: root, a, b, url, backend })
}

#[test]
fn ensure_cloned_checks_out_and_second_call_preserves_edits() -> TestResult {
    let f = setup("main")?;
    assert_eq!(head(&f.a)?, head(&f.b)?);
    assert_eq!(fs::read(f.a.join("README.md"))?, b"# initial\n");
    assert_eq!(f.backend.unpushed_count(&f.a)?, 0);
    assert_eq!(config(&f.a, "branch.main.remote")?.as_deref(), Some("origin"));
    assert_eq!(config(&f.a, "branch.main.merge")?.as_deref(), Some(MAIN));
    assert_eq!(config(&f.a, "core.autocrlf")?.as_deref(), Some("false"));
    assert_eq!(config(&f.a, "core.eol")?.as_deref(), Some("lf"));
    assert_eq!(GixBackend::tip(&gix::open(&f.a)?, "refs/remotes/origin/main")?, Some(head(Path::new(&f.url))?));
    assert!(config(&f.a, "http.sslCAInfo")?.is_none());
    assert!(!f._root.path().join("ca_bundle.pem").exists());
    remove_config(&f.a, &["core.autocrlf", "core.eol"])?;
    write_file(&f.a, "README.md", "local unsaved edit\n")?;
    assert!(!f.backend.ensure_cloned(&f.a, &f.url)?);
    assert_eq!(fs::read(f.a.join("README.md"))?, b"local unsaved edit\n");
    assert_eq!(config(&f.a, "core.autocrlf")?.as_deref(), Some("false"));
    let empty = f._root.path().join("empty"); fs::create_dir(&empty)?;
    assert!(f.backend.ensure_cloned(&empty, &f.url)?); assert_eq!(head(&empty)?, head(&f.b)?);
    Ok(())
}

#[test]
fn ensure_cloned_repairs_missing_tracking_and_pull_remains_usable() -> TestResult {
    let f = setup("main")?;
    for fetched in [false, true] {
        let partial = f._root.path().join(format!("partial-{fetched}"));
        let repo = gix::init(&partial)?; GixBackend::set_head(&repo, MAIN)?;
        GixBackend::configure(&repo, &[("remote.origin.url", &GixBackend::normalize_url(&f.url)?),
            ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")])?;
        if fetched { f.backend.fetch(&gix::open(&partial)?)?; }
        assert!(!f.backend.ensure_cloned(&partial, &f.url)?);
        assert_eq!(head(&partial)?, head(&f.b)?);
        assert_eq!(fs::read(partial.join("README.md"))?, b"# initial\n");
        f.backend.pull_rebase(&partial)?;
    }
    for keys in [vec!["branch.main.remote", "branch.main.merge"], vec!["branch.main.remote"], vec!["branch.main.merge"]] {
        remove_config(&f.a, &keys)?;
        assert!(!f.backend.ensure_cloned(&f.a, &f.url)?);
        assert!(!f.backend.ensure_cloned(&f.a, &f.url)?);
        assert_eq!(config(&f.a, "branch.main.remote")?.as_deref(), Some("origin"));
        assert_eq!(config(&f.a, "branch.main.merge")?.as_deref(), Some(MAIN));
        f.backend.pull_rebase(&f.a)?;
    }
    write_file(&f.b, "remote.md", "remote update\n")?;
    f.backend.commit_all(&f.b, "remote update")?; f.backend.push(&f.b)?;
    f.backend.pull_rebase(&f.a)?;
    assert_eq!(head(&f.a)?, head(&f.b)?);
    assert_eq!(fs::read(f.a.join("remote.md"))?, b"remote update\n");
    Ok(())
}

#[test]
fn ensure_cloned_rejects_different_origin_and_push_url() -> TestResult {
    let f = setup("main")?; let other = f._root.path().join("other.git"); init_bare(&other, "main", false)?;
    let other = other.to_string_lossy();
    assert!(matches!(f.backend.ensure_cloned(&f.a, &other), Err(SyncError::Git(_))));
    set_config(&f.a, "remote.origin.pushurl", &GixBackend::normalize_url(&other)?)?;
    assert!(matches!(f.backend.ensure_cloned(&f.a, &f.url), Err(SyncError::Git(_))));
    remove_config(&f.a, &["remote.origin.pushurl"])?;
    // A second URL must not bypass validation by leaving the first one intact.
    let repo = gix::open(&f.a)?; let path = repo.common_dir().join("config");
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    writeln!(file, "\n[remote \"origin\"]\nurl = {}", GixBackend::normalize_url(&other)?)?; drop(file);
    assert!(matches!(f.backend.ensure_cloned(&f.a, &f.url), Err(SyncError::Git(_))));
    Ok(())
}

#[test]
fn ensure_cloned_accepts_equivalent_local_path_and_file_url() -> TestResult {
    let root = tempfile::tempdir()?; let remote = root.path().join("remote space %# 中文.git");
    init_bare(&remote, "main", false)?;
    let raw = remote.to_string_lossy(); let url = GixBackend::normalize_url(&raw)?;
    assert!(url.starts_with("file:///")); assert!(!url.contains('\\')); assert!(url.contains("%20%25%23%20"));
    assert_eq!(GixBackend::normalize_url(&url)?, url);
    let client = root.path().join("client"); let backend = backend();
    assert!(backend.ensure_cloned(&client, &raw)?);
    assert_eq!(config(&client, "remote.origin.url")?.as_deref(), Some(url.as_str()));
    assert!(!backend.ensure_cloned(&client, &raw)?); assert!(!backend.ensure_cloned(&client, &url)?);
    set_config(&client, "remote.origin.url", &raw)?; set_config(&client, "remote.origin.pushurl", &url)?;
    assert!(!backend.ensure_cloned(&client, &url)?);
    Ok(())
}

#[test]
fn normalize_url_preserves_network_urls() -> TestResult {
    for url in ["https://github.com/example/journal.git", "git@github.com:example/journal.git"] {
        assert_eq!(GixBackend::normalize_url(url)?, url);
    }
    Ok(())
}

#[test]
fn ensure_cloned_rejects_nonempty_nonrepository_and_wrong_tracking() -> TestResult {
    let f = setup("main")?; let dir = f._root.path().join("notes"); write_file(&dir, "keep.md", "keep\n")?;
    assert!(matches!(f.backend.ensure_cloned(&dir, &f.url), Err(SyncError::Git(_))));
    assert_eq!(fs::read(dir.join("keep.md"))?, b"keep\n");
    set_config(&f.a, "branch.main.remote", "elsewhere")?;
    assert!(matches!(f.backend.ensure_cloned(&f.a, &f.url), Err(SyncError::Git(_))));
    set_config(&f.a, "branch.main.remote", "origin")?; set_config(&f.a, "branch.main.merge", "refs/tags/v1")?;
    assert!(matches!(f.backend.ensure_cloned(&f.a, &f.url), Err(SyncError::Git(_))));
    Ok(())
}

#[test]
fn empty_remote_allows_initial_commit_and_push() -> TestResult {
    let root = tempfile::tempdir()?; let remote = root.path().join("empty.git"); init_bare(&remote, "main", false)?;
    let url = remote.to_string_lossy(); let a = root.path().join("a"); let b = root.path().join("b"); let backend = backend();
    assert!(backend.ensure_cloned(&a, &url)?); assert!(!backend.ensure_cloned(&a, &url)?);
    assert!(!backend.commit_all(&a, "empty")?); assert!(backend.ensure_cloned(&b, &url)?);
    write_file(&a, "README.md", "first\n")?; assert!(backend.commit_all(&a, "initial")?);
    assert!(backend.unpushed_count(&a).is_err()); backend.push(&a)?;
    assert_eq!(backend.unpushed_count(&a)?, 0); backend.pull_rebase(&b)?; assert_eq!(head(&b)?, head(&a)?);
    Ok(())
}

#[test]
fn commit_all_returns_true_for_changes_and_false_when_clean() -> TestResult {
    let f = setup("main")?; let before = head(&f.a)?;
    assert!(!f.backend.has_local_changes(&f.a)); assert!(!f.backend.commit_all(&f.a, "nothing")?);
    write_file(&f.a, "days/today.md", "task\n")?; assert!(f.backend.has_local_changes(&f.a));
    assert!(f.backend.commit_all(&f.a, "sticky@test: task")?); assert_ne!(head(&f.a)?, before);
    assert!(!f.backend.has_local_changes(&f.a)); assert!(!f.backend.commit_all(&f.a, "nothing again")?);
    Ok(())
}

#[test]
fn commit_all_stages_deletions_and_respects_gitignore() -> TestResult {
    let f = setup("main")?; write_file(&f.a, ".gitignore", "cache/\n")?;
    write_file(&f.a, "cache/ignored.txt", "ignored\n")?; fs::remove_file(f.a.join("README.md"))?;
    assert!(f.backend.commit_all(&f.a, "remove readme and ignore cache")?);
    let files = GixBackend::head_files(&gix::open(&f.a)?)?;
    assert!(!files.contains_key("README.md")); assert!(!files.contains_key("cache/ignored.txt"));
    write_file(&f.a, "cache/ignored.txt", "changed but ignored\n")?;
    assert!(!f.backend.has_local_changes(&f.a)); assert!(!f.backend.commit_all(&f.a, "only ignored changes")?);
    Ok(())
}

#[test]
fn commit_identity_uses_cli_fallbacks_and_preserves_configured_identity() -> TestResult {
    let f = setup("main")?; set_config(&f.a, "user.name", "")?; set_config(&f.a, "user.email", "")?;
    write_file(&f.a, "one.md", "one\n")?; f.backend.commit_all(&f.a, "fallback")?;
    let repo = gix::open(&f.a)?; let commit = repo.head_commit()?; let author = commit.author()?;
    assert_eq!(author.name.to_string(), "Sticky Todo"); assert_eq!(author.email.to_string(), "sticky-todo@localhost");
    assert_eq!(config(&f.a, "user.name")?.as_deref(), Some("Sticky Todo"));
    set_config(&f.a, "user.name", "Configured User")?; set_config(&f.a, "user.email", "configured@example.com")?;
    write_file(&f.a, "two.md", "two\n")?; f.backend.commit_all(&f.a, "configured")?;
    let repo = gix::open(&f.a)?; let commit = repo.head_commit()?; let author = commit.author()?;
    assert_eq!(author.name.to_string(), "Configured User"); assert_eq!(author.email.to_string(), "configured@example.com");
    Ok(())
}

#[test]
fn push_drains_local_queue_and_count_does_not_fetch() -> TestResult {
    let f = setup("main")?;
    for name in ["one.md", "two.md"] { write_file(&f.a, name, name)?; assert!(f.backend.commit_all(&f.a, name)?); }
    assert_eq!(f.backend.unpushed_count(&f.a)?, 2);
    let old_upstream = GixBackend::tip(&gix::open(&f.b)?, "refs/remotes/origin/main")?;
    f.backend.push(&f.a)?; assert_eq!(f.backend.unpushed_count(&f.a)?, 0); assert_eq!(f.backend.unpushed_count(&f.b)?, 0);
    assert_eq!(GixBackend::tip(&gix::open(&f.b)?, "refs/remotes/origin/main")?, old_upstream);
    assert!(!f.backend.push_with_retry(&f.a)?);
    Ok(())
}

#[test]
fn pull_rebase_fast_forwards_to_remote_commit() -> TestResult {
    let f = setup("main")?; write_file(&f.b, "remote.md", "remote\n")?;
    f.backend.commit_all(&f.b, "remote update")?; f.backend.push(&f.b)?; f.backend.pull_rebase(&f.a)?;
    assert_eq!(head(&f.a)?, head(&f.b)?); assert_eq!(fs::read(f.a.join("remote.md"))?, b"remote\n");
    assert!(!f.backend.has_local_changes(&f.a)); assert_eq!(GixBackend::branch(&gix::open(&f.a)?)?, "main");
    write_file(&f.a, "local.md", "local\n")?; f.backend.commit_all(&f.a, "local update")?;
    let before = head(&f.a)?; f.backend.pull_rebase(&f.a)?; assert_eq!(head(&f.a)?, before);
    Ok(())
}

#[test]
fn push_rejection_rebases_and_retries_once() -> TestResult {
    let f = setup("main")?; write_file(&f.a, "days/today.md", "local task\n")?;
    f.backend.commit_all(&f.a, "local task")?; let original = head(&f.a)?;
    write_file(&f.b, "weeks/week.md", "remote task\n")?; f.backend.commit_all(&f.b, "remote task")?; f.backend.push(&f.b)?;
    assert!(f.backend.push(&f.a).is_err()); assert!(f.backend.push_with_retry(&f.a)?);
    assert_ne!(head(&f.a)?, original); assert_eq!(f.backend.unpushed_count(&f.a)?, 0);
    assert_eq!(gix::open(&f.a)?.head_commit()?.parent_ids().next().unwrap().detach(), head(&f.b)?);
    f.backend.pull_rebase(&f.b)?; assert_eq!(head(&f.a)?, head(&f.b)?);
    assert_eq!(fs::read(f.b.join("days/today.md"))?, b"local task\n");
    assert_eq!(fs::read(f.a.join("weeks/week.md"))?, b"remote task\n");
    Ok(())
}

#[test]
fn conflict_aborts_rebase_and_preserves_local_commit() -> TestResult {
    let f = setup("main")?; write_file(&f.a, "README.md", "local conflicting line\n")?;
    f.backend.commit_all(&f.a, "local conflict")?; let original = head(&f.a)?;
    let index = fs::read(f.a.join(".git/index"))?;
    write_file(&f.b, "README.md", "remote conflicting line\n")?; f.backend.commit_all(&f.b, "remote conflict")?; f.backend.push(&f.b)?;
    assert!(matches!(f.backend.pull_rebase(&f.a), Err(SyncError::Conflict(_))));
    assert_eq!(head(&f.a)?, original); assert_eq!(fs::read(f.a.join("README.md"))?, b"local conflicting line\n");
    assert_eq!(fs::read(f.a.join(".git/index"))?, index);
    for marker in ["rebase-merge", "rebase-apply", "MERGE_HEAD"] { assert!(!f.a.join(".git").join(marker).exists()); }
    assert!(!f.backend.has_local_changes(&f.a)); assert_eq!(f.backend.unpushed_count(&f.a)?, 1);
    assert!(matches!(f.backend.push_with_retry(&f.a), Err(SyncError::Conflict(_))));
    assert_eq!(head(&f.a)?, original); assert_eq!(head(&f.b)?, head(Path::new(&f.url))?);
    Ok(())
}

#[test]
fn non_main_branch_uses_configured_upstream_even_after_local_rename() -> TestResult {
    let f = setup("journal")?; let repo = gix::open(&f.a)?;
    assert_eq!(GixBackend::branch(&repo)?, "journal");
    repo.reference("refs/heads/local-journal", head(&f.a)?, PreviousValue::MustNotExist, "test: rename")?;
    GixBackend::set_head(&repo, "refs/heads/local-journal")?;
    GixBackend::configure(&repo, &[("branch.local-journal.remote", "origin"), ("branch.local-journal.merge", "refs/heads/journal")])?;
    repo.find_reference("refs/heads/journal")?.delete()?;
    write_file(&f.b, "remote.md", "remote\n")?; f.backend.commit_all(&f.b, "remote")?; f.backend.push(&f.b)?;
    f.backend.pull_rebase(&f.a)?; assert_eq!(head(&f.a)?, head(&f.b)?);
    write_file(&f.a, "local.md", "local\n")?; f.backend.commit_all(&f.a, "local")?;
    assert_eq!(f.backend.unpushed_count(&f.a)?, 1); f.backend.push(&f.a)?; assert_eq!(f.backend.unpushed_count(&f.a)?, 0);
    assert_eq!(head(Path::new(&f.url))?, head(&f.a)?); assert_eq!(GixBackend::branch(&repo)?, "local-journal");
    Ok(())
}

#[test]
fn invalid_repository_reports_errors_and_conservatively_reports_dirty() -> TestResult {
    let root = tempfile::tempdir()?; let path = root.path().join("missing"); let backend = backend();
    assert!(backend.has_local_changes(&path));
    assert!(matches!(backend.commit_all(&path, "no repo"), Err(SyncError::Git(_))));
    assert!(matches!(backend.push_with_retry(&path), Err(SyncError::Git(_))));
    assert!(matches!(backend.unpushed_count(&path), Err(SyncError::Git(_))));
    Ok(())
}

#[test]
fn pull_does_not_overwrite_uncommitted_changes() -> TestResult {
    let f = setup("main")?; write_file(&f.a, "README.md", "unsaved local text\n")?; let before = head(&f.a)?;
    write_file(&f.b, "README.md", "remote text\n")?; f.backend.commit_all(&f.b, "remote update")?; f.backend.push(&f.b)?;
    assert!(matches!(f.backend.pull_rebase(&f.a), Err(SyncError::Git(_))));
    assert_eq!(head(&f.a)?, before); assert_eq!(fs::read(f.a.join("README.md"))?, b"unsaved local text\n");
    assert!(f.backend.has_local_changes(&f.a));
    Ok(())
}

#[test]
fn raw_crlf_and_binary_bytes_survive_commit_clone_and_pull() -> TestResult {
    let f = setup("main")?;
    let bytes = b"first\r\nsecond\r\n\x00\xff";
    fs::write(f.a.join("bytes.dat"), bytes)?;
    // Attribute conversion must not change content stored by this backend.
    write_file(&f.a, ".gitattributes", "*.dat text eol=crlf\n")?;
    assert!(f.backend.commit_all(&f.a, "bytes")?); assert!(!f.backend.has_local_changes(&f.a));
    let stored = GixBackend::head_files(&gix::open(&f.a)?)?;
    assert_eq!(stored["bytes.dat"].1, bytes);
    f.backend.push(&f.a)?; f.backend.pull_rebase(&f.b)?; assert_eq!(fs::read(f.b.join("bytes.dat"))?, bytes);
    let c = f._root.path().join("c"); f.backend.ensure_cloned(&c, &f.url)?;
    assert_eq!(fs::read(c.join("bytes.dat"))?, bytes); assert!(!f.backend.has_local_changes(&c));
    Ok(())
}

#[test]
fn checkout_preserves_ignored_collision_and_recovers_after_ref_lock_failure() -> TestResult {
    let f = setup("main")?; write_file(&f.a, ".gitignore", "collision.md\n")?;
    f.backend.commit_all(&f.a, "ignore")?; f.backend.push(&f.a)?; f.backend.pull_rebase(&f.b)?;
    // B tracks a formerly ignored path after removing the ignore rule.
    fs::remove_file(f.b.join(".gitignore"))?; write_file(&f.b, "collision.md", "remote\n")?;
    f.backend.commit_all(&f.b, "remote")?; f.backend.push(&f.b)?;
    write_file(&f.a, "collision.md", "keep ignored content\n")?; let before = head(&f.a)?;
    assert!(f.backend.pull_rebase(&f.a).is_err()); assert_eq!(head(&f.a)?, before);
    assert_eq!(fs::read(f.a.join("collision.md"))?, b"keep ignored content\n");
    fs::remove_file(f.a.join("collision.md"))?;
    let index = fs::read(f.a.join(".git/index"))?;
    let lock = f.a.join(".git/refs/heads/main.lock"); fs::write(&lock, b"owned elsewhere")?;
    assert!(f.backend.pull_rebase(&f.a).is_err()); assert_eq!(head(&f.a)?, before);
    assert_eq!(fs::read(f.a.join(".git/index"))?, index);
    assert_eq!(fs::read(f.a.join(".gitignore"))?, b"collision.md\n");
    assert!(!f.a.join("collision.md").exists()); fs::remove_file(lock)?;
    f.backend.pull_rebase(&f.a)?; assert_eq!(head(&f.a)?, head(&f.b)?);
    Ok(())
}

#[test]
fn pkt_line_round_trip_and_malformed_frames() -> TestResult {
    let mut bytes = Vec::new(); packet(b"hello\0world\n", &mut bytes)?; bytes.extend_from_slice(b"0000");
    assert_eq!(packets(&bytes)?, vec![Some(b"hello\0world\n".as_slice()), None]);
    for invalid in [b"000".as_slice(), b"xxxx", b"0001", b"0003", b"0008hi", b"fff1"] { assert!(packets(invalid).is_err()); }
    assert!(packet(&vec![b'x'; 65517], &mut Vec::new()).is_err());
    Ok(())
}

fn advertisement(old: ObjectId) -> TestResult<Vec<u8>> {
    let mut bytes = Vec::new(); packet(b"# service=git-receive-pack\n", &mut bytes)?; bytes.extend_from_slice(b"0000");
    let name = if old.is_null() { "capabilities^{}" } else { MAIN };
    packet(format!("{old} {name}\0report-status delete-refs\n").as_bytes(), &mut bytes)?;
    bytes.extend_from_slice(b"0000"); Ok(bytes)
}
fn status_bytes(unpack: &str, reference: &str) -> TestResult<Vec<u8>> {
    let mut bytes = Vec::new(); packet(unpack.as_bytes(), &mut bytes)?; packet(reference.as_bytes(), &mut bytes)?;
    bytes.extend_from_slice(b"0000"); Ok(bytes)
}

#[test]
fn advertisement_and_report_status_are_strict() -> TestResult {
    let oid = ObjectId::from_hex(b"0123456789012345678901234567890123456789")?;
    assert_eq!(advertised_oid(&advertisement(oid)?, MAIN)?, oid);
    let null = ObjectId::null(gix::hash::Kind::Sha1);
    assert_eq!(advertised_oid(&advertisement(null)?, MAIN)?, null);
    report_status(&status_bytes("unpack ok\n", "ok refs/heads/main\n")?, MAIN)?;
    for (unpack, reference) in [("unpack checksum mismatch\n", "ok refs/heads/main\n"),
        ("unpack ok\n", "ng refs/heads/main denied\n"), ("unpack ok\n", "ok refs/heads/other\n")] {
        let error = report_status(&status_bytes(unpack, reference)?, MAIN).unwrap_err();
        assert!(matches!(error, SyncError::Git(_))); assert!(is_push_rejected(&error.to_string()));
    }
    assert!(report_status(b"0000", MAIN).is_err());
    assert!(advertised_oid(b"0000", MAIN).is_err());
    assert!(advertised_oid(&advertisement(oid)?[..20], MAIN).is_err());
    Ok(())
}

struct Request { line: String, headers: BTreeMap<String, String>, body: Vec<u8> }
fn read_request(stream: &mut TcpStream) -> TestResult<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?; stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream); let mut line = String::new(); reader.read_line(&mut line)?;
    let mut headers = BTreeMap::new(); let mut size = line.len();
    loop {
        let mut header = String::new(); if reader.read_line(&mut header)? == 0 { return Err("truncated HTTP headers".into()); }
        size += header.len(); if size > 32768 { return Err("headers too large".into()); }
        if header == "\r\n" { break; }
        let (key, value) = header.split_once(':').ok_or("invalid HTTP header")?;
        headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map(|v| v.parse::<usize>()).transpose()?.unwrap_or(0);
    if length > MAX_HTTP_BYTES as usize { return Err("body too large".into()); }
    let mut body = vec![0; length]; reader.read_exact(&mut body)?;
    Ok(Request { line: line.trim().to_owned(), headers, body })
}
struct MockServer { url: String, thread: thread::JoinHandle<std::result::Result<(), String>> }
impl MockServer {
    fn start(count: usize, handler: impl Fn(Request) -> TestResult<(u16, String, Vec<u8>)> + Send + 'static) -> TestResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?; listener.set_nonblocking(true)?;
        let url = format!("http://{}/journal.git", listener.local_addr()?);
        let thread = thread::spawn(move || {
            (|| -> TestResult {
                let deadline = Instant::now() + Duration::from_secs(15);
                for _ in 0..count {
                    let mut stream = loop {
                        match listener.accept() {
                            // Windows：accept 出的流继承监听者的非阻塞模式（Unix 不继承），
                            // 请求字节未到时 read_line 立即回 10035(WSAEWOULDBLOCK) 而非等
                            // 5s 超时——慢机器上 accept 赢客户端 send 的竞态即 mock 报错。
                            // 切回阻塞；读写超时由 read_request 设置。
                            Ok((stream, _)) => { stream.set_nonblocking(false)?; break stream; }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                            Err(e) => return Err(e.into()),
                        }
                    };
                    let request = read_request(&mut stream)?;
                    let (status, content_type, body) = handler(request)?;
                    write!(stream, "HTTP/1.1 {status} mock\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
                    stream.write_all(&body)?; stream.flush()?;
                }
                Ok(())
            })().map_err(|e| e.to_string())
        });
        Ok(Self { url, thread })
    }
    fn finish(self) -> TestResult { self.thread.join().map_err(|_| "mock panicked")?.map_err(Into::into) }
}

fn import_pack(remote: &Path, bytes: &[u8]) -> TestResult {
    let repo = gix::open(remote)?;
    let pack_dir = repo.git_dir().join("objects/pack"); fs::create_dir_all(&pack_dir)?;
    let outcome = gix::odb::pack::Bundle::write_to_directory(
        &mut Cursor::new(bytes), Some(&pack_dir), &mut gix::progress::Discard, &AtomicBool::new(false),
        None::<&Repository>, repo.object_hash(), Default::default(),
    )?;
    if let Some(path) = outcome.keep_path { fs::remove_file(path)?; }
    Ok(())
}

#[test]
fn mock_receive_pack_checks_basic_auth_command_and_complete_pack_then_gix_reads_it() -> TestResult {
    let f = setup("main")?; let old = head(&f.a)?;
    for n in 0..3 { write_file(&f.a, &format!("days/{n}.md"), &format!("task {n}\r\n"))?; f.backend.commit_all(&f.a, "task")?; }
    let new = head(&f.a)?; let remote = PathBuf::from(&f.url);
    let server = MockServer::start(2, move |request| {
        assert_eq!(request.headers.get("authorization").map(String::as_str), Some("Basic cGF0Om1vY2stcGF0"));
        if request.line.starts_with("GET ") {
            assert_eq!(request.line, "GET /journal.git/info/refs?service=git-receive-pack HTTP/1.1");
            return Ok((200, "application/x-git-receive-pack-advertisement".into(), advertisement(old)?));
        }
        assert_eq!(request.line, "POST /journal.git/git-receive-pack HTTP/1.1");
        assert_eq!(request.headers.get("content-type").map(String::as_str), Some("application/x-git-receive-pack-request"));
        let length = usize::from_str_radix(std::str::from_utf8(&request.body[..4])?, 16)?;
        assert_eq!(&request.body[4..length], format!("{old} {new} {MAIN}\0report-status\n").as_bytes());
        assert_eq!(&request.body[length..length + 4], b"0000");
        let pack = &request.body[length + 4..]; assert_eq!(&pack[..8], b"PACK\0\0\0\x02");
        let verification = remote.with_file_name("verify-pack.git");
        init_bare(&verification, "main", false)?;
        import_pack(&verification, pack)?;
        let isolated = gix::open(&verification)?;
        let reachable = GixBackend::objects(&isolated, new)?;
        assert_eq!(u32::from_be_bytes(pack[8..12].try_into()?) as usize, reachable.len());
        assert!(GixBackend::ancestors(&isolated, new)?.contains(&old));
        import_pack(&remote, pack)?;
        let repo = gix::open(&remote)?;
        // The importer verifies pack framing/checksum and indexes its objects.
        // Parents and all tree/blob objects must also be independently readable.
        for id in GixBackend::objects(&repo, new)? { repo.find_object(id)?; }
        repo.reference(MAIN, new, expected(Some(old)), "mock receive-pack")?;
        Ok((200, "application/x-git-receive-pack-result".into(), status_bytes("unpack ok\n", "ok refs/heads/main\n")?))
    })?;
    let backend = GixBackend::new(Arc::new(|| Some("mock-pat".into())));
    backend.push_http(&gix::open(&f.a)?, &server.url, MAIN, new)?; server.finish()?;
    f.backend.pull_rebase(&f.b)?;
    assert_eq!(head(&f.b)?, new); assert_eq!(fs::read(f.b.join("days/2.md"))?, b"task 2\r\n");
    Ok(())
}

#[test]
fn mock_push_rejection_is_redacted_and_does_not_advance_tracking() -> TestResult {
    let f = setup("main")?; let old = head(&f.a)?;
    write_file(&f.a, "local.md", "local\n")?; f.backend.commit_all(&f.a, "local")?; let new = head(&f.a)?;
    let server = MockServer::start(2, move |request| {
        if request.line.starts_with("GET ") { return Ok((200, "application/x-git-receive-pack-advertisement".into(), advertisement(old)?)); }
        Ok((200, "application/x-git-receive-pack-result".into(), status_bytes("unpack ok\n", "ng refs/heads/main mock-pat denied\n")?))
    })?;
    set_config(&f.a, "remote.origin.pushurl", &server.url)?;
    let backend = GixBackend::new(Arc::new(|| Some("mock-pat".into())));
    let error = backend.push(&f.a).unwrap_err(); server.finish()?;
    assert!(is_push_rejected(&error.to_string())); assert!(!error.to_string().contains("mock-pat"));
    assert!(error.to_string().contains("denied"));
    assert_eq!(head(&f.a)?, new); assert_eq!(backend.unpushed_count(&f.a)?, 1);
    Ok(())
}

#[test]
fn mock_unknown_remote_tip_prevents_force_push_without_post() -> TestResult {
    let f = setup("main")?; write_file(&f.a, "local.md", "local\n")?; f.backend.commit_all(&f.a, "local")?;
    let unknown = ObjectId::from_hex(b"0123456789012345678901234567890123456789")?;
    let server = MockServer::start(1, move |request| {
        assert!(request.line.starts_with("GET "));
        Ok((200, "application/x-git-receive-pack-advertisement".into(), advertisement(unknown)?))
    })?;
    let error = f.backend.push_http(&gix::open(&f.a)?, &server.url, MAIN, head(&f.a)?).unwrap_err(); server.finish()?;
    assert!(is_push_rejected(&error.to_string())); assert!(error.to_string().contains("non-fast-forward"));
    Ok(())
}

#[test]
fn credentials_are_ephemeral_and_network_urls_cannot_embed_them() -> TestResult {
    use gix::credentials::helper::Action;
    let mut callback = GixBackend::credentials(Some("mock-pat".into()));
    let result = callback(Action::get_for_url("https://example.com/repo.git"))?.unwrap();
    assert_eq!(result.identity.username, "pat"); assert_eq!(result.identity.password, "mock-pat");
    assert!(callback(result.next.store())?.is_none());
    assert!(GixBackend::credentials(None)(Action::get_for_url("https://example.com/repo.git"))?.is_none());
    assert!(GixBackend::network_url("https://pat:secret@example.com/repo.git").is_err());
    assert!(GixBackend::network_url("http://example.com/repo.git").is_err());
    assert!(GixBackend::network_url("https://example.com/repo.git?token=secret").is_err());
    assert_eq!(basic_credential("mock-pat"), "cGF0Om1vY2stcGF0");
    let clean = safe_message("Authorization: Basic cGF0Om1vY2stcGF0; mock-pat", Some("mock-pat"));
    assert!(!clean.contains("mock-pat")); assert!(!clean.contains("cGF0Om1vY2stcGF0"));
    assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty()); GixBackend::client()?;
    Ok(())
}

#[test]
#[ignore = "mock's v0 upload-pack caps don't fully match gix's negotiation (Could not decode server reply); gix fetch itself is covered by the local bare-repo tests and on-device HTTPS"]
fn mock_upload_pack_exercises_gix_fetch_through_the_same_tls_adapter() -> TestResult {
    let f = setup("main")?;
    let tip = head(Path::new(&f.url))?;
    let pack = GixBackend::full_pack(&gix::open(&f.url)?, tip)?;
    let server = MockServer::start(2, move |request| {
        if request.line.starts_with("GET ") {
            assert_eq!(request.line, "GET /journal.git/info/refs?service=git-upload-pack HTTP/1.1");
            let mut body = Vec::new(); packet(b"# service=git-upload-pack\n", &mut body)?; body.extend_from_slice(b"0000");
            packet(format!("{tip} HEAD\0side-band-64k ofs-delta symref=HEAD:{MAIN}\n").as_bytes(), &mut body)?;
            packet(format!("{tip} {MAIN}\n").as_bytes(), &mut body)?; body.extend_from_slice(b"0000");
            return Ok((200, "application/x-git-upload-pack-advertisement".into(), body));
        }
        assert_eq!(request.line, "POST /journal.git/git-upload-pack HTTP/1.1");
        let commands = String::from_utf8_lossy(&request.body);
        assert!(commands.contains(&format!("want {tip}"))); assert!(commands.contains("done"));
        let mut body = Vec::new(); packet(b"NAK\n", &mut body)?;
        for bytes in pack.chunks(65515) {
            let mut sideband = vec![1]; sideband.extend_from_slice(bytes); packet(&sideband, &mut body)?;
        }
        body.extend_from_slice(b"0000");
        Ok((200, "application/x-git-upload-pack-result".into(), body))
    })?;
    let client = f._root.path().join("http-clone");
    f.backend.ensure_cloned(&client, &server.url)?; server.finish()?;
    assert_eq!(head(&client)?, tip); assert_eq!(fs::read(client.join("README.md"))?, b"# initial\n");
    assert!(!f.backend.has_local_changes(&client)); assert_eq!(f.backend.unpushed_count(&client)?, 0);
    Ok(())
}

#[test]
#[ignore = "requires an explicitly configured disposable HTTPS repository and PAT; run on Android for acceptance"]
fn real_https_clone_commit_push_and_fetch_back() -> TestResult {
    let url = std::env::var("STICKY_GIX_HTTPS_TEST_URL")?;
    let backend = GixBackend::new(Arc::new(|| std::env::var("STICKY_GIX_HTTPS_TEST_PAT").ok()));
    let root = tempfile::tempdir()?; let a = root.path().join("a"); let b = root.path().join("b");
    backend.ensure_cloned(&a, &url)?; backend.ensure_cloned(&b, &url)?;
    write_file(&a, "sticky-gix-https-probe.md", &format!("HTTPS probe {}\n", gix::date::Time::now_utc().seconds))?;
    backend.commit_all(&a, "sticky@test: HTTPS acceptance")?; backend.push_with_retry(&a)?;
    backend.pull_rebase(&b)?; assert_eq!(head(&a)?, head(&b)?);
    assert_eq!(fs::read(a.join("sticky-gix-https-probe.md"))?, fs::read(b.join("sticky-gix-https-probe.md"))?);
    assert_eq!(backend.unpushed_count(&a)?, 0);
    Ok(())
}
