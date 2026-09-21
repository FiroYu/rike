use app_lib::parser::{Category, Flag, Priority};
use app_lib::repo::FileKind;
use app_lib::store::{resolve_kind, StickyStore};
use std::path::PathBuf;

fn date(s: &str) -> chrono::NaiveDate { chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap() }
fn setup(name: &str) -> (PathBuf, StickyStore) {
    let root = std::env::temp_dir().join(format!("sticky-notebook-{name}-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("_templates")).unwrap();
    std::fs::write(root.join("_templates/day.md"), "# {{YYYY-MM-DD}}\n\n## 日任务\n").unwrap();
    std::fs::write(root.join("_templates/week.md"), "# {{YYYY}}-W{{WW}}\n\n## 本周任务\n").unwrap();
    let store = StickyStore::new(&root);
    (root, store)
}

#[test]
fn legacy_files_remain_unchanged_and_appear_in_week_without_materialization() {
    let (root, store) = setup("legacy");
    std::fs::create_dir_all(root.join("days")).unwrap();
    let original = "# 2026-09-10\r\n\r\n## 日任务\r\n- [ ] (P1) [工作] 保留人工注记 #doing\r\n  子任务\r\n";
    std::fs::write(root.join("days/2026-09-10.md"), original).unwrap();
    let books = store.list_notebooks().unwrap();
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].name, "工作");
    let view = store.get_view("default", FileKind::Week(date("2026-09-12"))).unwrap();
    assert_eq!(view.tasks.len(), 1);
    assert_eq!(view.tasks[0].source.date, "2026-09-10");
    assert!(view.tasks[0].doing);
    assert_eq!(view.sources.len(), 8);
    assert_eq!(view.base_version, 0);
    assert_eq!(std::fs::read_to_string(root.join("days/2026-09-10.md")).unwrap(), original);
    assert!(!root.join("weeks").exists());
    assert!(!root.join("notebooks").exists());
}

#[test]
fn all_daily_states_roundtrip_through_week_source_including_delete_undo() {
    let (_, store) = setup("states");
    let day = FileKind::Day(date("2026-09-10"));
    let week = FileKind::Week(date("2026-09-12"));
    store.add_task("default", &day, Category::Work, Priority::P1, "同一条事项", &[], 0).unwrap();
    let view = store.get_view("default", week).unwrap();
    let task = &view.tasks[0];
    let source = resolve_kind(&task.source.kind, Some(&task.source.date)).unwrap();
    let line = task.line_idx;
    let result = store.set_flag("default", &source, line, Flag::Doing, true, task.source.base_version).unwrap();
    assert!(store.get_view("default", day).unwrap().tasks[0].doing);
    let result = store.set_blocked("default", &source, line, Some("待回复"), result.base_version).unwrap();
    let result = store.set_content("default", &source, line, "修改后的事项", None, result.base_version).unwrap();
    let result = store.set_priority("default", &source, line, Some(Priority::P0), result.base_version).unwrap();
    let result = store.set_category("default", &source, line, Some(Category::Personal), result.base_version).unwrap();
    let result = store.set_checked("default", &source, line, true, result.base_version).unwrap();
    let daily = store.get_view("default", day).unwrap();
    let weekly = store.get_view("default", week).unwrap();
    assert_eq!(serde_json::to_value(&daily.tasks[0]).unwrap(), serde_json::to_value(&weekly.tasks[0]).unwrap());
    assert!(daily.tasks[0].checked);
    assert!(!daily.tasks[0].doing);
    assert_eq!(daily.tasks[0].content, "修改后的事项");
    assert_eq!(daily.tasks[0].blocked_reason.as_deref(), Some("待回复"));
    assert_eq!(store.set_checked("default", &source, line, false, task.source.base_version).unwrap_err().code, "stale");
    let deleted = store.delete_task("default", &source, line, result.base_version).unwrap();
    assert!(store.get_view("default", week).unwrap().tasks.is_empty());
    store.restore_deleted("default", &source, line, deleted.removed, deleted.base_version).unwrap();
    assert!(store.get_view("default", week).unwrap().tasks[0].checked);
}

#[test]
fn iso_week_boundaries_same_line_and_identical_text_do_not_merge_tasks() {
    let (_, store) = setup("boundaries");
    for d in ["2025-12-28", "2025-12-29", "2026-01-01", "2026-01-04", "2026-01-05"] {
        store.add_task("default", &FileKind::Day(date(d)), Category::Work, Priority::P1, "重复文字", &[], 0).unwrap();
    }
    let week = resolve_kind("week", Some("2026-01-01")).unwrap();
    store.add_task("default", &week, Category::Work, Priority::P1, "重复文字", &[], 0).unwrap();
    let view = store.get_view("default", week).unwrap();
    assert_eq!(view.tasks.len(), 4);
    assert_eq!(view.tasks.iter().map(|t| t.source.date.as_str()).collect::<Vec<_>>(),
        ["2026-01-01", "2025-12-29", "2026-01-01", "2026-01-04"]);
    assert!(view.tasks.iter().all(|t| t.line_idx == view.tasks[0].line_idx));
    let t = &view.tasks[2];
    store.set_checked("default", &FileKind::Day(date(&t.source.date)), t.line_idx, true, t.source.base_version).unwrap();
    assert_eq!(store.get_view("default", week).unwrap().tasks.iter().filter(|t| t.checked).count(), 1);
    assert!(store.get_view("default", FileKind::Week(date("2026-01-05"))).unwrap().tasks.len() == 1);
}

#[test]
fn notebooks_isolate_daily_weekly_leftovers_and_survive_rename_and_restart() {
    let (root, store) = setup("isolation");
    let project = store.save_notebook(None, "  独立项目  ", 0).unwrap();
    let personal = store.save_notebook(None, "个人", 0).unwrap();
    let day = FileKind::Day(date("2026-09-10"));
    let week = FileKind::Week(date("2026-09-10"));
    for id in ["default", &project.id, &personal.id] {
        store.add_task(id, &day, Category::Work, Priority::P1, "相同内容", &[], 0).unwrap();
    }
    let view = store.get_view(&project.id, day).unwrap();
    store.set_checked(&project.id, &day, view.tasks[0].line_idx, true, view.base_version).unwrap();
    assert!(store.get_view(&project.id, week).unwrap().tasks[0].checked);
    assert!(!store.get_view(&personal.id, week).unwrap().tasks[0].checked);
    assert!(!store.get_view("default", week).unwrap().tasks[0].checked);
    let tomorrow = date("2026-09-11");
    assert_eq!(store.get_yesterday_leftovers(&project.id, tomorrow).unwrap().count, 0);
    let leftovers = store.get_yesterday_leftovers(&personal.id, tomorrow).unwrap();
    assert_eq!(leftovers.count, 1);
    store.copy_leftover_to_today(&personal.id, tomorrow, leftovers.tasks[0].line_idx, 0).unwrap();
    assert!(store.get_view("default", FileKind::Day(tomorrow)).unwrap().tasks.is_empty());
    assert_eq!(store.get_view(&personal.id, week).unwrap().tasks.len(), 2);
    let renamed = store.save_notebook(Some(&project.id), "副业", project.base_version.parse().unwrap()).unwrap();
    assert_eq!(renamed.id, project.id);
    assert_eq!(store.save_notebook(Some(&project.id), "旧名称覆盖", project.base_version.parse().unwrap()).unwrap_err().code, "stale");
    let reopened = StickyStore::new(root);
    assert_eq!(reopened.list_notebooks().unwrap().len(), 3);
    assert!(reopened.get_view(&project.id, week).unwrap().tasks[0].checked);
}

#[test]
fn notebook_validation_and_watchers_cover_all_scopes() {
    let (root, store) = setup("validation");
    for id in ["../days", "C:/temp", "nb/other", "", "missing"] {
        assert!(store.get_view(id, FileKind::Day(date("2026-09-10"))).is_err());
        assert!(store.add_task(id, &FileKind::Day(date("2026-09-10")), Category::Work, Priority::P1, "无效写入", &[], 0).is_err());
    }
    for name in ["", "  ", "a\nb", "工作"] { assert!(store.save_notebook(None, name, 0).is_err()); }
    assert!(store.save_notebook(None, &"字".repeat(41), 0).is_err());
    let before = store.watched_fingerprints(date("2026-09-12"));
    let book = store.save_notebook(None, "个人", 0).unwrap();
    assert_ne!(before, store.watched_fingerprints(date("2026-09-12")));
    let before = store.watched_fingerprints(date("2026-09-12"));
    store.add_task(&book.id, &FileKind::Day(date("2025-01-01")), Category::Personal, Priority::P2, "历史任务", &[], 0).unwrap();
    assert_ne!(before, store.watched_fingerprints(date("2026-09-12")));
    assert!(!root.join("days").exists());
    assert!(!root.join("_current.md").exists());
    assert_eq!(store.save_notebook(Some("default"), "原有事项", 0).unwrap().name, "原有事项");
    assert!(store.get_view("default", FileKind::Day(date("2025-01-01"))).unwrap().tasks.is_empty());
}

/// 真实同步仓落盘的 notebook.json 通过公共 API 可读（互通：安卓端复用同格式）。
#[test]
fn reads_real_notebook_metadata_fixture() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/notebooks/independent.json"),
    ).unwrap();
    let tmp = std::env::temp_dir().join(format!("sticky-nbmeta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let dir = tmp.join("notebooks").join("nb-fixture");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notebook.json"), text).unwrap();
    let dto = app_lib::notebook::read(&tmp, "nb-fixture").unwrap();
    assert_eq!(dto.name, "个人");
    let _ = std::fs::remove_dir_all(&tmp);
}
