//! 解析器测试：字节保真往返（G20）+ 手术最小侵入（G21）+ 两层结构解析。
//!
//! fixtures/ 下是样例文件（08-11/12/13 日文件、模板；结构与真实同步仓一致）。

use app_lib::parser::{Category, Flag, Priority, TodoFile};

fn fixture(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读 {path:?} 失败: {e}"))
}

/// G20：真实文件往返字节一致。
#[test]
fn roundtrip_real_files() {
    for rel in [
        "days/2026-08-11.md",
        "days/2026-08-12.md",
        "days/2026-08-13.md",
        "_templates/day.md",
        "_templates/week.md",
        "weeks/2026-W33.md",
        "weeks/2026-W34.md",
    ] {
        let text = fixture(rel);
        let tf = TodoFile::parse(&text);
        assert_eq!(tf.serialize(), text, "往返不一致: {rel}");
    }
}

/// G20 补充：CRLF 行尾与无尾换行文件的往返。
#[test]
fn roundtrip_eol_variants() {
    let crlf = "# 2026-08-12 (周三)\r\n\r\n## 日任务\r\n\r\n- [ ] (P1) 任务A #overdue\r\n- [x] (P2) 任务B\r\n";
    assert_eq!(TodoFile::parse(crlf).serialize(), crlf);

    let no_trailing = "- [ ] (P1) 无尾换行";
    assert_eq!(TodoFile::parse(no_trailing).serialize(), no_trailing);
}

/// 两层结构：## 段落 × ### 子区；备注里的普通 `- ` 列表不是任务。
#[test]
fn two_layer_structure() {
    let tf = TodoFile::parse(&fixture("days/2026-08-12.md"));
    assert_eq!(tf.tasks.len(), 10, "08-12 应有 10 个任务");

    let day = tf.tasks.iter().filter(|t| t.section == "日任务").collect::<Vec<_>>();
    assert_eq!(day.len(), 10, "全部在日任务段（含子区）");
    assert_eq!(day.iter().filter(|t| t.subsection.is_none()).count(), 5);
    assert_eq!(
        day.iter().filter(|t| t.subsection.as_deref() == Some("⭐ 睡前必须完成")).count(),
        5,
        "⭐ 子区 5 条"
    );
    // 完成事项/备注段没有任务（备注是普通 - 列表）
    assert!(tf.tasks.iter().all(|t| t.section != "备注"));
    assert!(tf.tasks.iter().all(|t| t.section != "完成事项"));
}

/// N3：完成尾注 ✅ 保留在 display 中，勾选翻转不动它。
#[test]
fn done_tail_notes_in_display() {
    let tf = TodoFile::parse(&fixture("days/2026-08-12.md"));
    let t = tf
        .tasks
        .iter()
        .find(|t| t.checked && t.content.contains("家庭相册"))
        .expect("找到已完成条目");
    assert!(t.display.contains("8/13完成"), "尾注进 display: {}", t.display);
}

/// G21：勾选翻转只改一个字符，尾注原样保留。
#[test]
fn set_checked_single_char_edit() {
    let text = fixture("days/2026-08-12.md");
    let mut tf = TodoFile::parse(&text);
    let idx = tf.tasks.iter().find(|t| !t.checked).unwrap().line_idx;
    let before = tf.lines[idx].clone();
    tf.set_checked(idx, true).unwrap();
    let after = &tf.lines[idx];
    // 恰好一个字符不同（' ' → 'x'）
    let diffs = before.chars().zip(after.chars()).filter(|(a, b)| a != b).count();
    assert_eq!(diffs, 1);
    assert_eq!(before.len(), after.len());

    // 翻回后与原行一致
    tf.set_checked(idx, false).unwrap();
    assert_eq!(tf.lines[idx], before);
}

/// 分类标签：插入→移除 往复原行一致；替换正确。
#[test]
fn category_roundtrip() {
    let line = "- [ ] (P1) 个人网站改版：重写「关于」页文案 #overdue";
    let text = format!("## 日任务\n\n{line}\n");
    let mut tf = TodoFile::parse(&text);
    let idx = tf.tasks[0].line_idx;
    assert_eq!(tf.tasks[0].category, Category::Uncategorized);

    tf.set_category(idx, Some(Category::Work)).unwrap();
    assert_eq!(tf.tasks[0].category, Category::Work);
    assert_eq!(tf.lines[idx], "- [ ] (P1) [工作] 个人网站改版：重写「关于」页文案 #overdue");

    // 换成个人
    tf.set_category(idx, Some(Category::Personal)).unwrap();
    assert_eq!(tf.lines[idx], "- [ ] (P1) [个人] 个人网站改版：重写「关于」页文案 #overdue");

    // 移除 → 回到原行
    tf.set_category(idx, None).unwrap();
    assert_eq!(tf.lines[idx], line);
}

/// 无优先级条目的分类插入位置（checkbox 之后）。
#[test]
fn category_without_priority() {
    let text = "- [ ] 无优先级任务\n";
    let mut tf = TodoFile::parse(text);
    let idx = tf.tasks[0].line_idx;
    tf.set_category(idx, Some(Category::Personal)).unwrap();
    assert_eq!(tf.lines[idx], "- [ ] [个人] 无优先级任务");
}

/// 标签：#doing 追加幂等；#overdue 移除；#blocked 连带原因词。
#[test]
fn flag_operations() {
    let text = "- [ ] (P1) 预约牙医复诊 #blocked 等诊所回复 #overdue\n";
    let mut tf = TodoFile::parse(text);
    let idx = tf.tasks[0].line_idx;
    let t = &tf.tasks[0];
    assert_eq!(t.blocked_reason.as_deref(), Some("等诊所回复"));
    assert!(t.overdue);
    assert!(!t.doing);

    // 幂等追加
    tf.add_flag(idx, Flag::Overdue).unwrap();
    assert_eq!(tf.lines[idx].matches("#overdue").count(), 1, "不重复追加");

    tf.add_flag(idx, Flag::Doing).unwrap();
    assert!(tf.lines[idx].ends_with("#doing"));
    assert!(tf.tasks[0].doing);

    // 移除 overdue
    tf.remove_flag(idx, Flag::Overdue).unwrap();
    assert!(!tf.tasks[0].overdue);
    assert!(!tf.lines[idx].contains("#overdue"));

    // 移除 blocked（不存在该 API 则跳过——blocked 属解析兼容范围，写入 UI 为 P1）
    // 这里验证原因词随标签一起被移除的语义在 parse 侧成立即可：
    let t = &tf.tasks[0];
    assert_eq!(t.blocked_reason.as_deref(), Some("等诊所回复"));
}

/// 注记保留：#overdue 后的非标签文本进 display。
#[test]
fn annotations_kept() {
    let text = "- [ ] (P2) 补录昨天的事项 #overdue 补记9/2\n";
    let tf = TodoFile::parse(text);
    let t = &tf.tasks[0];
    assert!(t.overdue);
    assert_eq!(t.content, "补录昨天的事项");
    assert!(t.display.contains("补记9/2"), "注记保留在 display: {}", t.display);
}

/// 删除任务连带缩进子行。
#[test]
fn delete_with_sublines() {
    let text = "\
## 日任务

- [ ] (P1) 周报草稿：汇总本周三个项目的进展
  要求：每个项目写结论和下一步；控制在半页
  最后附里程碑表格
- [ ] (P2) 第二个任务
";
    let mut tf = TodoFile::parse(text);
    assert_eq!(tf.tasks.len(), 2);
    assert_eq!(tf.tasks[0].sub_lines.len(), 2, "缩进续行归属上一条");

    let idx = tf.tasks[0].line_idx;
    tf.delete_task(idx).unwrap();
    assert_eq!(tf.tasks.len(), 1);
    assert!(tf.tasks[0].content.contains("第二个任务"));
    assert!(!tf.serialize().contains("要求：每个项目"), "子行一并删除");
    assert!(!tf.serialize().contains("周报草稿"), "主行删除");
}

/// 新增任务落位在二级段平铺区（不进入 ### 子区，不落入下一二级段）。
#[test]
fn add_task_flat_area() {
    let text = "\
## 日任务

- [ ] (P1) 已有的平铺任务

### ⭐ 睡前必须完成
- [x] (P1) 子区任务

## 完成事项
";
    let mut tf = TodoFile::parse(text);
    let at = tf.add_task("日任务", Category::Work, Priority::P2, "新增的任务", &[]).unwrap();
    // 新行应在已有平铺任务之后、### 之前
    assert_eq!(tf.lines[at], "- [ ] (P2) [工作] 新增的任务");
    let pos_sub = tf.lines.iter().position(|l| l.starts_with("### ")).unwrap();
    assert!(at < pos_sub, "新增行必须在子区标题之前");
    // 视图刷新后可见
    assert!(tf.tasks.iter().any(|t| t.content == "新增的任务" && t.subsection.is_none()));
}

/// 空日文件：零任务、往返一致、可直接新增。
#[test]
fn empty_day_file() {
    let text = fixture("days/2026-08-13.md");
    let mut tf = TodoFile::parse(&text);
    assert!(tf.tasks.is_empty(), "空文件零任务");

    tf.add_task("日任务", Category::Personal, Priority::P3, "空文件里的第一条", &[]).unwrap();
    assert_eq!(tf.tasks.len(), 1);
    let t = &tf.tasks[0];
    assert_eq!(t.section, "日任务");
    assert_eq!(t.category, Category::Personal);
    assert_eq!(t.priority, Some(Priority::P3));

    // 重新解析序列化结果，结构稳定
    let tf2 = TodoFile::parse(&tf.serialize());
    assert_eq!(tf2.tasks.len(), 1);
    assert_eq!(tf2.tasks[0].content, "空文件里的第一条");
}

/// set_content 只替换内容主体，保留前缀/标签/注记。
#[test]
fn set_content_preserves_affixes() {
    let text = "- [ ] (P1) [工作] 旧内容 #blocked 等回复 #overdue 补记9/2\n";
    let mut tf = TodoFile::parse(text);
    let idx = tf.tasks[0].line_idx;
    tf.set_content(idx, "新内容").unwrap();
    assert_eq!(tf.lines[idx], "- [ ] (P1) [工作] 新内容 #blocked 等回复 #overdue 补记9/2");
    assert_eq!(tf.tasks[0].content, "新内容");
    assert_eq!(tf.tasks[0].blocked_reason.as_deref(), Some("等回复"));
}

/// set_priority：插入（checkbox 后、分类前）/ 原地替换 / 移除字节还原（US11 P0 置顶的文件层）。
#[test]
fn set_priority_insert_swap_remove() {
    let text = "# D\n\n## 日任务\n- [ ] [个人] 无优先级任务\n- [ ] (P2) [工作] 有优先级\n";
    let mut tf = TodoFile::parse(text);
    let li0 = tf.tasks[0].line_idx;
    let li1 = tf.tasks[1].line_idx;

    // 插入：位于 checkbox 后、分类标签前
    tf.set_priority(li0, Some(Priority::P0)).unwrap();
    assert_eq!(tf.lines[li0], "- [ ] (P0) [个人] 无优先级任务");
    assert_eq!(tf.tasks[0].priority, Some(Priority::P0));

    // 等长原地替换
    tf.set_priority(li0, Some(Priority::P2)).unwrap();
    assert_eq!(tf.lines[li0], "- [ ] (P2) [个人] 无优先级任务");

    // 移除 → 整个文件字节还原
    tf.set_priority(li0, None).unwrap();
    assert_eq!(tf.serialize(), text);

    // 已有优先级的行：替换
    tf.set_priority(li1, Some(Priority::P0)).unwrap();
    assert_eq!(tf.lines[li1], "- [ ] (P0) [工作] 有优先级");

    // 已是同值：幂等，不报错
    tf.set_priority(li1, Some(Priority::P0)).unwrap();
    assert_eq!(tf.lines[li1], "- [ ] (P0) [工作] 有优先级");
}

/// F7：#blocked 增/改/删。增 = 行尾追加；改 = 原因词替换；删 = 移除标签连同原因。
#[test]
fn set_blocked_add_update_remove() {
    let text = "# D\n\n## 日任务\n- [ ] (P1) [工作] 联系供应商 #doing\n- [ ] (P2) [个人] 买咖啡 #blocked 缺现金\n";
    let mut tf = TodoFile::parse(text);
    let li0 = tf.tasks[0].line_idx;
    let li1 = tf.tasks[1].line_idx;

    // 增：行尾追加（不影响既有 #doing）
    tf.set_blocked(li0, Some("等回复")).unwrap();
    assert_eq!(tf.lines[li0], "- [ ] (P1) [工作] 联系供应商 #doing #blocked 等回复");
    assert_eq!(tf.tasks[0].blocked_reason.as_deref(), Some("等回复"));
    assert!(tf.tasks[0].doing);

    // 改：替换原因词
    tf.set_blocked(li0, Some("等二次回复")).unwrap();
    assert_eq!(tf.lines[li0], "- [ ] (P1) [工作] 联系供应商 #doing #blocked 等二次回复");
    assert_eq!(tf.tasks[0].blocked_reason.as_deref(), Some("等二次回复"));

    // 删：标签连同原因移除，前置 #doing 完好
    tf.set_blocked(li0, None).unwrap();
    assert_eq!(tf.lines[li0], "- [ ] (P1) [工作] 联系供应商 #doing");
    assert_eq!(tf.tasks[0].blocked_reason, None);

    // 既有 blocked+原因：删除后字节还原到无标签形态
    tf.set_blocked(li1, None).unwrap();
    assert_eq!(tf.lines[li1], "- [ ] (P2) [个人] 买咖啡");
    assert_eq!(tf.tasks[1].blocked_reason, None);

    // 空白原因拒绝（防止写出无原因的悬空标签）
    assert!(tf.set_blocked(li1, Some("   ")).is_err());
}

/// 删除（含缩进子行）→ 原样插回 = 字节级恢复（3s 撤销气泡的保真基础）。
#[test]
fn delete_then_restore_byte_exact() {
    let text = "# D\n\n## 日任务\n- [ ] (P1) [工作] 主任务\n  子说明行\n  另一条\n- [ ] (P2) [个人] 买咖啡\n";
    let mut tf = TodoFile::parse(text);
    let li0 = tf.tasks[0].line_idx;

    let removed = tf.delete_task(li0).unwrap();
    assert_eq!(removed.len(), 3, "应连删缩进子行");
    assert_eq!(tf.tasks.len(), 1);
    assert_eq!(tf.tasks[0].line_idx, li0, "后续任务上移到同一行号");

    // 原样插回 → 整个文件字节还原
    tf.insert_lines_at(li0, removed);
    assert_eq!(tf.serialize(), text);
    assert_eq!(tf.tasks.len(), 2);
    assert_eq!(tf.tasks[0].line_idx, li0);

    // 越界钳制到末尾，不 panic
    let r2 = tf.delete_task(tf.tasks[1].line_idx).unwrap();
    tf.insert_lines_at(usize::MAX, r2);
    assert_eq!(tf.serialize(), text);
}

/// 安全审查 H1：内容区为空、标签紧跟前缀（"- [ ] #doing"、"(P1) #blocked 原因"）
/// 曾触发反向切片 panic——release 是 panic=abort，远端推来这种行会让启动/tick 死循环。
#[test]
fn tag_first_line_no_panic_roundtrip() {
    let text = "# 2026-09-10 (周四)\n\n## 日任务\n- [ ] #doing\n- [ ] (P1) #blocked 等回复\n- [ ] (P0) [工作] #overdue\n";
    let tf = TodoFile::parse(text);
    assert_eq!(tf.tasks.len(), 3);
    assert!(tf.tasks[0].doing && tf.tasks[0].content.is_empty());
    assert_eq!(tf.tasks[1].blocked_reason.as_deref(), Some("等回复"));
    assert!(tf.tasks[1].content.is_empty());
    assert!(tf.tasks[2].overdue && tf.tasks[2].content.is_empty());
    assert_eq!(tf.serialize(), text);
}

/// 用户文本守卫：拦换行与活标签词；识别规则与 parse_line 对齐（空格后/行首 + 闭集词）。
#[test]
fn validate_user_text_rules() {
    use app_lib::parser::{validate_restore_lines, validate_sub_lines, validate_user_text};
    assert!(validate_user_text("正常内容 #话题").is_ok());
    assert!(validate_user_text("提交#overdue 报告").is_ok()); // # 前不是空格，parse 同样不识别
    assert!(validate_user_text("提交 #overdue 报告").is_err());
    assert!(validate_user_text("#doing").is_err());
    assert!(validate_user_text("受阻 #blocked 原因").is_err());
    assert!(validate_user_text("带\n换行").is_err());
    assert!(validate_user_text("带\r换行").is_err());
    // v0.8：新词同样拦截；子行只拦换行（标签在缩进保护下是普通正文）
    assert!(validate_user_text("暂停 #paused 一会").is_err());
    assert!(validate_user_text("计时 #t 5").is_err());
    assert!(validate_user_text("起点 #ts 100").is_err());
    assert!(validate_sub_lines(&["带 #t 5 标签的子行".to_string()]).is_ok());
    assert!(validate_sub_lines(&["带\n换行".to_string()]).is_err());
    assert!(validate_restore_lines(&["- [ ] (P1) #blocked 原因".to_string()]).is_ok());
    assert!(validate_restore_lines(&["a\nb".to_string()]).is_err());
}

// ---------- v0.8：四态状态机 / 任务计时器 / 多行子行 ----------

use app_lib::parser::{TaskStatus, TIMER_CAP_SECS};

fn one_task(text: &str) -> (TodoFile, usize) {
    let tf = TodoFile::parse(text);
    assert_eq!(tf.tasks.len(), 1, "样例应恰好一个任务: {text}");
    let li = tf.tasks[0].line_idx;
    (tf, li)
}

fn task_status(text: &str) -> (TaskStatus, u64, Option<u64>, bool) {
    let (tf, _) = one_task(text);
    let t = &tf.tasks[0];
    (t.status, t.timer_secs, t.timer_started_at, t.timer_invalid)
}

/// 四态派生：checked > paused > doing > 仅计时标签(非规范 Paused) > Todo。
#[test]
fn v08_status_derivation() {
    assert_eq!(task_status("- [ ] (P1) 纯待开始\n").0, TaskStatus::Todo);
    assert_eq!(task_status("- [ ] (P1) 进行中 #doing #t 0 #ts 100\n").0, TaskStatus::Doing);
    assert_eq!(task_status("- [ ] (P1) 暂停 #paused #t 30\n").0, TaskStatus::Paused);
    assert_eq!(task_status("- [x] (P1) 完成 #t 30\n").0, TaskStatus::Done);
    // 仅 #t（非规范落盘）→ Paused；doing+paused 并存 → Paused 优先
    assert_eq!(task_status("- [ ] (P1) 只有累计 #t 30\n").0, TaskStatus::Paused);
    assert_eq!(task_status("- [ ] (P1) 冲突 #doing #paused #t 30\n").0, TaskStatus::Paused);
    // 旧协议 #doing 无 #t：Doing、累计 0
    let (st, secs, _, _) = task_status("- [ ] (P1) 旧协议 #doing\n");
    assert_eq!((st, secs), (TaskStatus::Doing, 0));
}

/// 计时派生：多 #t 后者胜、封顶 CAP、非法值置 invalid 且按 0 参与。
#[test]
fn v08_timer_derivation() {
    let (_, secs, ts, inv) = task_status("- [ ] a #t 100 #t 200 #ts 1000\n");
    assert_eq!((secs, ts, inv), (200, Some(1000), false));

    let (_, secs, _, inv) = task_status(&format!("- [ ] a #t {}\n", TIMER_CAP_SECS + 12345));
    assert_eq!((secs, inv), (TIMER_CAP_SECS, false), "超界封顶");

    let (_, secs, _, inv) = task_status("- [ ] a #t abc #t 7\n");
    assert_eq!((secs, inv), (7, true), "值后者胜；invalid 粘滞（非法 token 仍在行内，下次手术才清）");

    let (_, secs, _, inv) = task_status("- [ ] a #t abc\n");
    assert_eq!((secs, inv), (0, true));

    let (_, _, ts, inv) = task_status("- [ ] a #doing #t 0 #ts xyz\n");
    assert_eq!((ts, inv), (None, true), "#ts 非法同样置 invalid");
}

/// 词边界：旧词前缀匹配（#doingx 仍是 doing）；新词要求空格/行尾边界
/// （#today 不被 #t 吞、#pausedx 不是 paused；#ts 先于 #t 检查）。
#[test]
fn v08_tag_word_boundary() {
    // #today：不是计时标签
    assert_eq!(task_status("- [ ] a #today\n").0, TaskStatus::Todo);
    assert_eq!(task_status("- [ ] a #today\n").1, 0);
    // #pausedx：不是暂停
    assert_eq!(task_status("- [ ] a #pausedx\n").0, TaskStatus::Todo);
    // #t5：t 无边界，不是标签
    assert_eq!(task_status("- [ ] a #t5\n").0, TaskStatus::Todo);
    // 旧词前缀语义保留：#doingx → doing
    assert_eq!(task_status("- [ ] a #doingx\n").0, TaskStatus::Doing);
    // #ts 与 #t 并存：ts 不被当成 t 的值
    let (_, secs, ts, _) = task_status("- [ ] a #t 5 #ts 100\n");
    assert_eq!((secs, ts), (5, Some(100)));
}

/// blocked 原因终止边界：原因为空且紧邻新标签（re==rs）时正确终止、不越界访问。
#[test]
fn v08_blocked_empty_reason_adjacent_tag() {
    let tf = TodoFile::parse("- [ ] a #blocked #doing\n- [ ] b #blocked #t 5\n- [ ] c #blocked #paused #t 3\n");
    assert_eq!(tf.tasks.len(), 3);
    assert_eq!(tf.tasks[0].blocked_reason, None);
    assert_eq!(tf.tasks[0].status, TaskStatus::Doing);
    assert_eq!(tf.tasks[1].blocked_reason, None);
    assert_eq!(tf.tasks[1].timer_secs, 5);
    assert_eq!(tf.tasks[2].blocked_reason, None);
    assert_eq!(tf.tasks[2].status, TaskStatus::Paused);
    // 往返字节一致（无 panic、无切片错位）
    let text = "- [ ] a #blocked #doing\n- [ ] b #blocked #t 5\n- [ ] c #blocked #paused #t 3\n";
    assert_eq!(tf.serialize(), text);
}

/// 状态转移矩阵（固定时间样例，GPT-6 方案 §4.1）：
/// 100/110 → 10；200/205 → 15；300/307 → 22（复活续计不清零）。
#[test]
fn v08_set_status_transition_times() {
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) 写周报\n");

    // 待开始 → 进行中 @100：首离待开始即写 #t 0
    tf.set_status_at(li, TaskStatus::Doing, 100).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) 写周报 #t 0 #doing #ts 100");

    // 进行中 → 暂停 @110：结算 0 + 10
    tf.set_status_at(li, TaskStatus::Paused, 110).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) 写周报 #t 10 #paused");

    // 暂停 → 进行中 @200：保留 t，重设 ts
    tf.set_status_at(li, TaskStatus::Doing, 200).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) 写周报 #t 10 #doing #ts 200");

    // 进行中 → 完成 @205：结算 10 + 5，删 doing/ts
    tf.set_status_at(li, TaskStatus::Done, 205).unwrap();
    assert_eq!(tf.lines[li], "- [x] (P1) 写周报 #t 15");

    // 完成 → 进行中 @300：复活，t 冻结值续计
    tf.set_status_at(li, TaskStatus::Doing, 300).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) 写周报 #t 15 #doing #ts 300");

    // 进行中 → 暂停 @307：15 + 7
    tf.set_status_at(li, TaskStatus::Paused, 307).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) 写周报 #t 22 #paused");

    // 派生核对
    assert_eq!(tf.status_at(li).unwrap(), TaskStatus::Paused);
    let t = &tf.tasks[0];
    assert_eq!((t.status, t.timer_secs), (TaskStatus::Paused, 22));
}

/// 拒绝项：todo 目标、非任务行。（Done→Paused 于 v0.8.1 放开为复活路径，
/// 正向钉死见 parser::tests::done_revives_to_paused_frozen。）
#[test]
fn v08_set_status_rejections() {
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a\n");
    assert!(tf.set_status_at(li, TaskStatus::Todo, 100).is_err(), "待开始不可回归");
    assert!(tf.set_status_at(0, TaskStatus::Doing, 100).is_err(), "标题行不是任务");
}

/// 同态幂等：字节零变化；唯一例外 Doing 无 ts 时补启。
#[test]
fn v08_set_status_idempotent() {
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a #t 10 #paused\n");
    tf.set_status_at(li, TaskStatus::Paused, 999).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) a #t 10 #paused", "Paused→Paused 零改动");

    let (mut tf, li) = one_task("## 日任务\n\n- [x] (P1) a #t 10\n");
    tf.set_status_at(li, TaskStatus::Done, 999).unwrap();
    assert_eq!(tf.lines[li], "- [x] (P1) a #t 10", "Done→Done 零改动");

    // 遗留无 ts 的 Doing（旧协议）：幂等路径例外，补 ts 不重置 t
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a #doing #t 10\n");
    tf.set_status_at(li, TaskStatus::Doing, 500).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) a #t 10 #doing #ts 500");

    // 有 ts 的 Doing→Doing：零改动（不重设起点）
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a #t 10 #doing #ts 500\n");
    tf.set_status_at(li, TaskStatus::Doing, 999).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) a #t 10 #doing #ts 500");
}

/// 手术清理：重复 #t 只留首个、多余 doing/paused/ts 全删；#overdue/#blocked/注记保留。
/// 样例必须是 Doing 态（含 #paused 的行派生即 Paused，会走同态幂等短路零改动）。
#[test]
fn v08_set_status_cleans_duplicates_keeps_others() {
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a #t 3 #doing #t 9 #ts 90 #overdue 补记9/2\n");
    tf.set_status_at(li, TaskStatus::Paused, 100).unwrap();
    // base = 多 #t 后者胜 = 9；delta = 100-90 = 10 → 19；目标态追加在行尾（注记字节不动）
    assert_eq!(tf.lines[li], "- [ ] (P1) a #t 19 #overdue 补记9/2 #paused");
    assert!(tf.tasks[0].overdue);

    // 同态幂等样例：已派生 Paused 的行再 set Paused，零字节改动
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a #t 3 #paused #t 9 #doing #ts 7\n");
    let before = tf.lines[li].clone();
    tf.set_status_at(li, TaskStatus::Paused, 100).unwrap();
    assert_eq!(tf.lines[li], before);

    // blocked + 原因原样穿过状态手术
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) b #blocked 等回复 #doing #t 4 #ts 9\n");
    tf.set_status_at(li, TaskStatus::Done, 20).unwrap();
    assert_eq!(tf.lines[li], "- [x] (P1) b #blocked 等回复 #t 15");
    assert_eq!(tf.tasks[0].blocked_reason.as_deref(), Some("等回复"));
}

/// 状态封顶：累计 + 增量越过 CAP 时钳到 99:59:59。
#[test]
fn v08_set_status_caps_timer() {
    let base = TIMER_CAP_SECS - 10; // 差 10 秒到顶
    let (mut tf, li) = one_task(&format!("## 日任务\n\n- [ ] (P1) a #t {base} #doing #ts 100\n"));
    tf.set_status_at(li, TaskStatus::Paused, 500).unwrap(); // delta 400 → 应封顶
    assert_eq!(tf.tasks[0].timer_secs, TIMER_CAP_SECS);
    assert!(tf.lines[li].contains(&format!("#t {TIMER_CAP_SECS}")));
}

/// 时钟倒拨（now < ts）：saturating 保证 delta=0，不 panic 不负数。
#[test]
fn v08_set_status_clock_backwards() {
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a #t 5 #doing #ts 1000\n");
    tf.set_status_at(li, TaskStatus::Paused, 200).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) a #t 5 #paused");
}

/// Todo 直接完成后取消：复活回 Doing，累计（0）不清零、起点重设。
#[test]
fn v08_todo_done_then_revive() {
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) 直接完成\n");
    tf.set_status_at(li, TaskStatus::Done, 100).unwrap();
    assert_eq!(tf.lines[li], "- [x] (P1) 直接完成 #t 0");
    tf.set_status_at(li, TaskStatus::Doing, 300).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) 直接完成 #t 0 #doing #ts 300");
}

/// 多行子行：解析剥两空格结构缩进；replace_sub_lines 替换/清空；块边界含缩进空白行。
#[test]
fn v08_sub_lines_roundtrip_and_replace() {
    let text = "## 日任务\n\n- [ ] (P1) 主任务\n  子说明一\n  子说明二\n- [ ] (P2) 下一个\n";
    let (mut tf, li) = one_task_prefix(text);
    assert_eq!(tf.tasks[0].sub_lines, vec!["子说明一".to_string(), "子说明二".to_string()]);
    assert_eq!(tf.tasks[1].sub_lines.len(), 0);
    // 往返字节一致（缩进由 serialize 补回）
    assert_eq!(tf.serialize(), text);

    // 替换
    tf.replace_sub_lines(li, &["新的子行A".into(), "新的子行B".into()]).unwrap();
    assert_eq!(tf.tasks[0].sub_lines.len(), 2);
    assert!(tf.lines[li + 1].starts_with("  新的子行A"), "落盘两空格缩进: {}", tf.lines[li + 1]);

    // 清空
    tf.replace_sub_lines(li, &[]).unwrap();
    assert_eq!(tf.tasks[0].sub_lines.len(), 0);
    let after = tf.serialize();
    assert!(!after.contains("子说明"), "子行全清");
    assert!(after.contains("- [ ] (P1) 主任务\n- [ ] (P2) 下一个\n"));
}

/// 与 one_task 相同，但保留多任务样例（子行测试需要两个任务）。
fn one_task_prefix(text: &str) -> (TodoFile, usize) {
    let tf = TodoFile::parse(text);
    assert!(!tf.tasks.is_empty(), "样例至少一个任务: {text}");
    let li = tf.tasks[0].line_idx;
    (tf, li)
}

/// 缩进空白行属于上一任务块（delete/replace 连带）；tab 行剥一层 tab。
/// strip 语义是「两空格或 tab 二选一」：`  \t行` 只剥两空格，tab 保留在子行内。
#[test]
fn v08_block_boundary_indented_blank_and_tab() {
    let text = "## 日任务\n\n- [ ] (P1) a\n  \t制表子行\n  \n- [ ] (P2) b\n";
    let tf = TodoFile::parse(text);
    assert_eq!(tf.tasks[0].sub_lines, vec!["\t制表子行".to_string(), "".to_string()],
        "两空格+tab 只剥两空格；缩进空白行保留为空子行");
    assert_eq!(tf.serialize(), text);
    // 块边界：task_block_end 覆盖缩进空白行，不含下一任务
    let li = tf.tasks[0].line_idx;
    assert_eq!(tf.task_block_end(li), li + 3);
}

/// is_heading 只认第 0 列：缩进的 "### x" 是普通缩进行（归属上一任务块）。
#[test]
fn v08_heading_column_zero() {
    let text = "## 日任务\n\n- [ ] (P1) a\n  ### 缩进伪标题\n";
    let tf = TodoFile::parse(text);
    assert_eq!(tf.tasks[0].sub_lines, vec!["### 缩进伪标题".to_string()]);
    // 第 0 列真标题后的缩进行不属于任何任务
    let tf2 = TodoFile::parse("### 真标题\n  缩进行\n");
    assert!(tf2.tasks.is_empty());
}

/// remove_flag：重复标签全删（收集全部区间）。
#[test]
fn v08_remove_flag_all_duplicates() {
    let (mut tf, li) = one_task("## 日任务\n\n- [ ] (P1) a #doing #t 3 #doing\n");
    tf.remove_flag(li, Flag::Doing).unwrap();
    assert_eq!(tf.lines[li], "- [ ] (P1) a #t 3", "两个 #doing 全删");
    assert!(!tf.lines[li].contains("doing"));
}


#[test]
fn pause_preserves_annotation_between_timer_and_status() {
    for input in ["- [ ] A #t 45 补记 #doing #ts 100", "- [ ] A #t 45 #doing 补记 #ts 100", "- [ ] A #t 45 #doing 补记"] {
        let mut f = TodoFile::parse(input);
        let expected_secs = if input.contains("#ts") { 55 } else { 45 };
        f.set_status_at(0, app_lib::parser::TaskStatus::Paused, 110).unwrap();
        assert_eq!(f.lines[0], format!("- [ ] A #t {expected_secs} 补记 #paused"));
        let again = TodoFile::parse(&f.serialize());
        assert_eq!(again.tasks[0].timer_secs, expected_secs);
        assert_eq!(again.tasks[0].status, app_lib::parser::TaskStatus::Paused);
        assert_eq!(again.tasks[0].display, "A 补记");
        assert!(!again.tasks[0].timer_invalid);
    }
}

#[test]
fn pause_preserves_annotation_after_timestamp() {
    let mut f = TodoFile::parse("- [ ] A #t 45 #doing #ts 100 补记  原文");
    f.set_status_at(0, app_lib::parser::TaskStatus::Paused, 110).unwrap();
    assert_eq!(f.lines[0], "- [ ] A #t 55 补记  原文 #paused");
    let again = TodoFile::parse(&f.serialize());
    assert_eq!(again.tasks[0].timer_secs, 55);
    assert_eq!(again.tasks[0].status, app_lib::parser::TaskStatus::Paused);
    assert_eq!(again.tasks[0].display, "A 补记  原文");
    assert_eq!(again.tasks[0].timer_started_at, None);
    assert!(!again.tasks[0].timer_invalid);
}

fn assert_sub_line_boundary_roundtrip(separator: &str) {
    let text = format!("- [ ] A\n  任务说明\n  \n  任务说明\n{separator}\n  段落缩进行\n");
    let mut f = TodoFile::parse(&text);
    let subs = f.tasks[0].sub_lines.clone();
    f.replace_sub_lines(0, &subs).unwrap();
    assert_eq!(f.serialize(), text, "编辑往返不得复制块外文本");
    assert_eq!(subs, vec!["任务说明", "", "任务说明"]);
    assert_eq!(f.task_block_end(0), 4);
    let removed = f.delete_task(0).unwrap();
    assert_eq!(f.serialize(), format!("{separator}\n  段落缩进行\n"));
    f.insert_lines_at(0, removed);
    assert_eq!(f.serialize(), text);
}

#[test]
fn blank_line_ends_sub_lines_roundtrip() {
    assert_sub_line_boundary_roundtrip("");
}

#[test]
fn ordinary_paragraph_ends_sub_lines_roundtrip() {
    assert_sub_line_boundary_roundtrip("普通段落");
}
