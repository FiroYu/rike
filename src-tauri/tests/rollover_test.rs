//! 跨日流转（Win 专属）集成测试：MOVE 语义、范围边界、字段保真、
//! 同文去重与崩溃收敛、多笔记本、幂等重入、模板建文件条件。
//!
//! 不在测试里改进程级环境变量（STICKY_AUTO_ROLLOVER 开关走代码评审，
//! 与既有 STICKY_REPO_URL 的隔离纪律一致）。

#![cfg(windows)]

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use app_lib::parser::{Category, Priority};
use app_lib::repo::FileKind;
use app_lib::rollover::carry_over;
use app_lib::store::StickyStore;

fn d(s: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn git(cwd: &Path, args: &[&str]) {
    let mut cmd = std::process::Command::new("git");
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(cwd)
        .output()
        .expect("git 可执行");
    assert!(
        out.status.success(),
        "git {args:?} 失败: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

const DAY_TEMPLATE: &str = "# {{YYYY-MM-DD}} (周{{WEEKDAY_CN}})\n\n> 本日重点: \n\n## 日任务\n\n## 完成事项\n\n## 备注\n";
const WEEK_TEMPLATE: &str = "# {{YYYY}}-W{{WW}} ({{START}} ~ {{END}})\n\n> 本周目标: \n\n## 本周任务\n";

/// 造一个日文件：行列表 = ## 日任务 段的内容。
fn day_file(lines: &[&str]) -> String {
    format!("# 日期\n\n> 本日重点: \n\n## 日任务\n{}\n## 完成事项\n\n## 备注\n", lines.join("\n"))
}

/// bare 远端 + 克隆（含模板）。返回克隆路径。
fn setup_repo(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sticky-rollover-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let remote = base.join("remote.git");
    let a = base.join("a");
    git(&base, &["init", "--bare", &remote.to_string_lossy()]);
    let url = format!("file:///{}", remote.to_string_lossy().replace('\\', "/"));
    git(&base, &["clone", &url, &a.to_string_lossy()]);
    git(&a, &["config", "user.name", "Test"]);
    git(&a, &["config", "user.email", "test@example.com"]);
    write(&a, "_templates/day.md", DAY_TEMPLATE);
    write(&a, "_templates/week.md", WEEK_TEMPLATE);
    git(&a, &["add", "-A"]);
    git(&a, &["commit", "-m", "init"]);
    a
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap()
}

#[test]
fn moves_unchecked_to_today_and_prunes_history() {
    let a = setup_repo("core");
    write(&a, "days/2026-09-15.md", &day_file(&[
        "- [ ] (P1) [工作] 任务A",
        "- [x] (P1) [工作] 任务B",
        "- [ ] (P2) [个人] 任务C",
        "  子行备注",
        "- [x] (P3) 任务D",
        "- [ ] 任务E #overdue 补记9/15",
    ]));
    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((report.moved, report.merged), (3, 0), "A、C(+子行)、E 三项搬入");
    assert_eq!(store.take_pending_edits(), 3);

    let today = read(&a, "days/2026-09-16.md");
    assert!(today.contains("任务A"), "今天应含任务A: {today}");
    assert!(today.contains("任务C"));
    assert!(today.contains("子行备注"), "缩进子行随块搬运");
    assert!(today.contains("任务E"));
    assert!(today.contains("补记9/15"), "#overdue 后的注记保留");
    assert!(!today.contains("#overdue"), "目的不继承旧日期状态");

    let yesterday = read(&a, "days/2026-09-15.md");
    assert!(yesterday.contains("任务B") && yesterday.contains("任务D"), "已完成项原地保留");
    for gone in ["任务A", "任务C", "任务E", "子行备注"] {
        assert!(!yesterday.contains(gone), "昨日不应再有 {gone}: {yesterday}");
    }
    // F14 昨日遗留随即归零
    let leftovers = store.get_yesterday_leftovers("default", d("2026-09-16")).unwrap();
    assert_eq!(leftovers.count, 0);
}

#[test]
fn multi_day_multi_notebook_and_scope_boundaries() {
    let a = setup_repo("scope");
    write(&a, "days/2026-09-10.md", &day_file(&["- [ ] 旧任务甲"]));
    write(&a, "days/2026-09-14.md", &day_file(&["- [ ] 中任务乙", "- [ ] 中任务丙"]));
    write(&a, "days/2026-09-16.md", &day_file(&["- [ ] 今日已有"]));
    write(&a, "days/2026-09-17.md", &day_file(&["- [ ] 未来任务"]));
    write(&a, "weeks/2026-W37.md", "# 2026-W37 (2026-09-07 ~ 2026-09-13)\n\n> 本周目标: \n\n## 本周任务\n");
    write(&a, "notes/2026-09-15.md", "随记一条\n");

    let store = StickyStore::new(a.clone());
    let nb = store.save_notebook(None, "独立项目", 0).unwrap();
    write(&a, &format!("notebooks/{}/days/2026-09-15.md", nb.id), &day_file(&["- [ ] 本内任务"]));

    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!(report.moved, 4, "默认本 3 + 独立本 1");

    let today = read(&a, "days/2026-09-16.md");
    assert!(today.contains("旧任务甲") && today.contains("中任务乙") && today.contains("中任务丙"));
    // 日期升序：旧任务在中任务之前
    assert!(today.find("旧任务甲").unwrap() < today.find("中任务乙").unwrap());
    assert_eq!(today.matches("- [ ]").count(), 4);

    // 未来日、周文件、速记不动；独立本的历史任务进入独立本的今天
    assert!(read(&a, "days/2026-09-17.md").contains("未来任务"));
    assert!(read(&a, "notes/2026-09-15.md").contains("随记一条"));
    let nb_today = read(&a, &format!("notebooks/{}/days/2026-09-16.md", nb.id));
    assert!(nb_today.contains("本内任务"));
    assert!(nb_today.starts_with("# 2026-09-16"), "独立本今日从模板创建");

    // 幂等重入
    let again = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((again.moved, again.merged), (0, 0));
    assert_eq!(read(&a, "days/2026-09-16.md"), today, "重入不改变今日文件");
}

#[test]
fn preserves_fields_and_other_sections_stay() {
    let a = setup_repo("fields");
    // 备注区任务：手工放置在 ## 备注 段的未勾选任务，不应被搬
    let mut text = day_file(&[
        "- [ ] (P0) [工作] 保留优先级 #doing",
        "- [ ] 保留无优先级 #blocked 等外部",
        "  嵌套子行",
        "- [ ] 搬我",
        "- [x] 已完成不搬",
    ]);
    text = text.replace("\n## 完成事项\n\n## 备注\n", "\n## 完成事项\n\n## 备注\n- [ ] (P1) 备注区任务\n");
    write(&a, "days/2026-09-15.md", &text);

    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!(report.moved, 3, "仅 ## 日任务 段的未勾选任务搬运");

    let today = read(&a, "days/2026-09-16.md");
    assert!(today.contains("(P0) [工作] 保留优先级 #doing"), "优先级/分类/#doing 原样保留");
    assert!(today.contains("- [ ] 保留无优先级 #blocked 等外部"), "无优先级与 blocked 原因保留");
    assert!(today.contains("嵌套子行"));
    assert!(!today.contains("备注区任务"), "其他章节的任务不动");
    let yesterday = read(&a, "days/2026-09-15.md");
    assert!(yesterday.contains("已完成不搬"));
    assert!(yesterday.contains("备注区任务"));
}

#[test]
fn same_content_merges_and_partial_state_converges() {
    let a = setup_repo("dedup");
    let original_y = day_file(&["- [ ] 任务A", "- [ ] 任务F"]);
    write(&a, "days/2026-09-15.md", &original_y);
    write(&a, "days/2026-09-16.md", &day_file(&["- [ ] 任务A"]));

    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((report.moved, report.merged), (1, 1), "F 搬入，A 并入今日同文");
    let today = read(&a, "days/2026-09-16.md");
    assert_eq!(today.matches("- [ ]").count(), 2, "任务A 只有一份: {today}");
    assert!(today.contains("任务F"));
    assert!(!read(&a, "days/2026-09-15.md").contains("- [ ]"), "昨日未勾选清空");

    // 崩溃中间态模拟：源文件回滚到搬运前（目标已写、源写丢失）
    write(&a, "days/2026-09-15.md", &original_y);
    let reconverge = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((reconverge.moved, reconverge.merged), (0, 2), "A、F 均并入，不重复加");
    assert!(!read(&a, "days/2026-09-15.md").contains("- [ ]"), "源最终清空");
    assert_eq!(read(&a, "days/2026-09-16.md").matches("- [ ]").count(), 2, "收敛后仍两份（A+F）");
}

#[test]
fn missing_today_file_created_only_when_moving() {
    let a = setup_repo("template");
    // 只有已完成任务的昨日：无候选 → 不建今日文件
    write(&a, "days/2026-09-14.md", &day_file(&["- [x] 完成了"]));
    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!(report.moved, 0);
    assert!(!a.join("days/2026-09-16.md").exists(), "无搬运不建文件（G5）");

    // 有未完成 → 今日从模板创建并套上日期头
    write(&a, "days/2026-09-15.md", &day_file(&["- [ ] 任务A"]));
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!(report.moved, 1);
    let today = read(&a, "days/2026-09-16.md");
    assert!(today.starts_with("# 2026-09-16"), "模板占位符已填充: {today}");
    assert!(today.contains("## 日任务"));
    assert!(today.contains("任务A"));
}

/// 未来日期预排的完整生命周期（v0.7.0）：浏览未来日不建文件 →
/// 预排任务留在未来日 → 到期日文件即今日文件（不复制）→ 过期后
/// 未完成项流转入当天、已完成项留原日、当天已有预排不被误伤。
#[test]
fn future_tasks_stay_until_due_then_roll_over() {
    let a = setup_repo("future-life");
    // D < T < U：T = 预排日；carry_over(today) 的 today 依次取 D、T、U
    // 模拟「今天」逐日推进，无需真实午夜。
    let t_kind = FileKind::Day(d("2027-01-01"));
    let u_kind = FileKind::Day(d("2027-01-02"));
    let store = StickyStore::new(a.clone());

    // 1) 浏览未来日：视图为空、版本 0，磁盘不建文件
    let view = store.get_view("default", t_kind).unwrap();
    assert!(!view.exists && view.base_version == 0 && view.tasks.is_empty());
    assert!(!a.join("days/2027-01-01.md").exists(), "浏览不建文件");

    // 2) 真实 add_task 在 T 预排两项，完成其一；U 预排一项
    let r1 = store.add_task("default", &t_kind, Category::Uncategorized, Priority::P1, "预排任务甲", &[], 0).unwrap();
    let r2 = store.add_task("default", &t_kind, Category::Uncategorized, Priority::P1, "预排任务乙", &[], r1.base_version).unwrap();
    store.set_checked("default", &t_kind, r2.line_idx, true, r2.base_version).unwrap();
    store.add_task("default", &u_kind, Category::Uncategorized, Priority::P1, "U日原有任务", &[], 0).unwrap();
    let t_before = read(&a, "days/2027-01-01.md");
    let u_before = read(&a, "days/2027-01-02.md");
    assert!(t_before.starts_with("# 2027-01-01"), "未来文件套当日模板: {t_before}");

    // 3) 未到期与到期当日：流转零搬运，两文件字节不变，D 文件不创建
    let before_due = carry_over(&store, d("2026-12-31")).unwrap();
    assert_eq!((before_due.moved, before_due.merged), (0, 0), "未到期不动");
    let on_due = carry_over(&store, d("2027-01-01")).unwrap();
    assert_eq!((on_due.moved, on_due.merged), (0, 0), "到期当日不搬自己");
    assert_eq!(read(&a, "days/2027-01-01.md"), t_before);
    assert_eq!(read(&a, "days/2027-01-02.md"), u_before);
    assert!(!a.join("days/2026-12-31.md").exists(), "无搬运不建 D 文件");

    // 4) T 过期（今天=U）：未完成的甲搬入 U，已完成的乙留 T，U 原有任务保留
    let past = carry_over(&store, d("2027-01-02")).unwrap();
    assert_eq!((past.moved, past.merged), (1, 0));
    let u_after = read(&a, "days/2027-01-02.md");
    assert!(u_after.contains("预排任务甲") && u_after.contains("U日原有任务"), "U 收甲且保留原有: {u_after}");
    assert!(!u_after.contains("预排任务乙"), "已完成项不搬: {u_after}");
    let t_after = read(&a, "days/2027-01-01.md");
    assert!(t_after.contains("预排任务乙") && !t_after.contains("- [ ]"), "完成项留 T: {t_after}");

    // 5) 幂等重入
    let again = carry_over(&store, d("2027-01-02")).unwrap();
    assert_eq!((again.moved, again.merged), (0, 0));
}

// ---------- v0.8：同任务判定升级（计时/暂停/子行等富信息保护） ----------

/// 同文但今日侧带计时（富）→ 不是同一任务：两份保留，不静默丢源数据。
#[test]
fn v08_same_content_rich_tasks_both_kept() {
    let a = setup_repo("v08-rich");
    write(&a, "days/2026-09-16.md", &day_file(&["- [ ] 任务A #t 100 #paused"]));
    write(&a, "days/2026-09-15.md", &day_file(&["- [ ] 任务A"]));
    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((report.moved, report.merged), (1, 0), "同文但今日带计时：不并入");
    let today = read(&a, "days/2026-09-16.md");
    assert_eq!(today.matches("- [ ] 任务A").count(), 2, "两份都保留: {today}");
    assert!(today.contains("#t 100 #paused"), "今日原任务计时不受影响");
    assert!(!read(&a, "days/2026-09-15.md").contains("- [ ]"), "源清空");
}

/// 剥 #overdue 后整块字节相等（含计时）→ 并入。
#[test]
fn v08_identical_rich_blocks_merge() {
    let a = setup_repo("v08-merge-rich");
    write(&a, "days/2026-09-16.md", &day_file(&["- [ ] 任务A #t 100 #paused"]));
    // 昨日同任务但带 #overdue（旧日期状态剥掉后与今日整块相等）
    write(&a, "days/2026-09-15.md", &day_file(&["- [ ] 任务A #t 100 #paused #overdue"]));
    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((report.moved, report.merged), (0, 1), "整块相等并入");
    let today = read(&a, "days/2026-09-16.md");
    assert_eq!(today.matches("- [ ] 任务A").count(), 1, "仍一份: {today}");
    assert!(!today.contains("#overdue"), "目的不继承旧日期状态");
}

/// 子行不同 → 两份保留；无同文的照常搬入。
#[test]
fn v08_sub_lines_difference_decides() {
    let a = setup_repo("v08-subs");
    write(&a, "days/2026-09-16.md", &day_file(&["- [ ] 任务A", "  备注-今日"]));
    write(&a, "days/2026-09-15.md", &day_file(&[
        "- [ ] 任务A",
        "  备注-昨日",
        "- [ ] 任务B",
        "  共同备注",
    ]));
    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((report.moved, report.merged), (2, 0), "A 子行不同两份保留；B 搬入");
    let today = read(&a, "days/2026-09-16.md");
    assert!(today.contains("备注-今日") && today.contains("备注-昨日"), "两份共存: {today}");
    assert!(today.contains("共同备注"), "B 的子行随搬");

    // 幂等重入（源已清空）
    let again = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((again.moved, again.merged), (0, 0));
}

/// 双方朴素同文（仅优先级不同）→ 仍并入（v0.7 内容级语义保留给朴素任务，今日版胜出）。
#[test]
fn v08_plain_tasks_still_merge_on_content() {
    let a = setup_repo("v08-plain");
    write(&a, "days/2026-09-16.md", &day_file(&["- [ ] (P2) 任务A"]));
    write(&a, "days/2026-09-15.md", &day_file(&["- [ ] (P1) 任务A"]));
    let store = StickyStore::new(a.clone());
    let report = carry_over(&store, d("2026-09-16")).unwrap();
    assert_eq!((report.moved, report.merged), (0, 1), "朴素同文并入");
    let today = read(&a, "days/2026-09-16.md");
    assert_eq!(today.matches("- [ ]").count(), 1, "仍一份: {today}");
    assert!(today.contains("(P2)"), "今日版本胜出: {today}");
}
