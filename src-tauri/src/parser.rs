//! 同步仓 markdown 解析与字节保真编辑。
//!
//! 设计原则（PRD「文件层最小侵入」）：
//! - `lines` 是唯一事实源；`tasks` 是派生视图，每次手术后单行重解析保持一致。
//! - 所有编辑只动目标 token 的字节区间，其余字节原样保留。
//! - 往返保证：parse → serialize 与原文件字节一致（测试 G20）。
//!
//! 条目语法（v0.8 状态协议，与仓库 CLAUDE.md 一字不差）：
//! ```text
//! - [ ] (P1) [工作] 任务内容 #blocked 原因 #overdue #doing #t 3600 #ts 1758000000
//! ```
//! - 分类标签：封闭词表 `[工作]`/`[个人]`，仅识别「checkbox+优先级前缀之后紧跟」的位置。
//! - `#blocked` 后跟原因词（直到下一个 ` #` 或行尾）；`#overdue`/`#doing` 独立；
//!   标签之后的非 `#` 文本为人工注记（如 `#overdue 补记9/2`），原样保留。
//! - v0.8 计时标签：`#t N` 累计秒（一旦离开待开始即写入，零值不可省略）、
//!   `#ts S` 本段计时起点（unix 秒，仅进行中存在）。规范落盘：
//!   进行中 `[ ] … #doing #t N #ts S`；暂停 `[ ] … #paused #t N`；完成 `[x] … #t N`。
//!   状态派生优先级：checked→完成 > #paused > #doing > 仅有 #t/#ts（非规范，按暂停）。

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Category {
    Work,
    Personal,
    Uncategorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::P0 => "P0",
            Priority::P1 => "P1",
            Priority::P2 => "P2",
            Priority::P3 => "P3",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Flag {
    Overdue,
    Doing,
}

/// v0.8 任务四态。Todo 是一次性初始态：一旦离开（落盘任何计时/状态标签或勾选）
/// 永不回归——`set_status` 命令层拒绝 todo 目标，此处仅供展示派生。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Todo,
    Doing,
    Paused,
    Done,
}

/// 计时上限 99:59:59（秒）。累计与实时值均封顶，防止前端格式化越界。
pub const TIMER_CAP_SECS: u64 = 359_999;

#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub line_idx: usize,
    pub checked: bool,
    pub priority: Option<Priority>,
    pub category: Category,
    /// 展示文本（内容 + 注记），不含状态标签
    pub display: String,
    /// 纯内容段（编辑时替换的部分）
    pub content: String,
    pub doing: bool,
    pub overdue: bool,
    pub blocked_reason: Option<String>,
    /// v0.8 派生四态
    pub status: TaskStatus,
    /// 已落盘的累计秒（`#t` 值，非法按 0、超界封顶；不含进行中的实时增量——
    /// 前端 ticker 在 Doing 且 `timer_started_at` 有效时再加 `now - ts`）
    pub timer_secs: u64,
    /// 本段计时起点（`#ts` 原始 unix 秒）
    pub timer_started_at: Option<u64>,
    /// `#t`/`#ts` 值无法解析为非负整数（按 0 参与计算；下次状态手术重写为合法值）
    pub timer_invalid: bool,
    /// 归属子行（缩进续行，如「老板要求: ...」）
    pub sub_lines: Vec<String>,
    /// 所在二级段（如「日任务」）
    pub section: String,
    /// 所在三级子区标题（如「⭐ 睡前必须完成」），无则 None
    pub subsection: Option<String>,
}

/// 一个任务行内部各 token 的字节区间（基于该行去掉行尾符的字符串）。
#[derive(Debug, Clone)]
struct Spans {
    cb: (usize, usize),                    // checkbox 内部字符
    prio: Option<(usize, usize)>,          // 含圆括号 (P1)
    cat: Option<(usize, usize)>,           // 含方括号 [工作]
    content: (usize, usize),               // 内容主体
    /// 已知状态标签，按出现顺序
    tags: Vec<TagSpan>,
}

#[derive(Debug, Clone)]
struct TagSpan {
    kind: FlagOrBlocked,
    span: (usize, usize),          // 标签本身（不含前导空格）
    reason: Option<(usize, usize)>, // #blocked 的原因词区间
    /// #t/#ts 的数值文本区间（不含前导空格；非法文本同样记录，编辑时整体重写）
    value: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FlagOrBlocked {
    Overdue,
    Doing,
    Blocked,
    Paused,
    Time,
    TimeStart,
}

#[derive(Debug)]
struct ParsedLine {
    checked: bool,
    priority: Option<Priority>,
    category: Category,
    content: String,
    display: String,
    doing: bool,
    overdue: bool,
    blocked_reason: Option<String>,
    status: TaskStatus,
    timer_base: u64,
    timer_started_at: Option<u64>,
    timer_invalid: bool,
    spans: Spans,
}

#[derive(Debug, Default)]
pub struct TodoFile {
    /// 去掉行尾符的每一行（唯一事实源）
    pub lines: Vec<String>,
    crlf: bool,
    trailing_newline: bool,
    /// 任务索引：tasks[i].line_idx 升序
    pub tasks: Vec<TaskView>,
    /// 每个任务的解析缓存，与 tasks 同下标
    parsed: Vec<ParsedLine>,
}

fn is_task_start(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() >= 6 && b.starts_with(b"- [") && (b[3] == b' ' || b[3] == b'x') && b[4] == b']'
        && (b.len() == 5 || b[5] == b' ' || b[5] == 0x0)
}

fn is_heading(line: &str) -> Option<u8> {
    // 标题仅认第 0 列（v0.8 修复）：缩进的「  ## 备注」是普通文本/子行，
    // 不重置段落归属——原 trim_start 会把任务块后的缩进标题误判为段落边界。
    if line.starts_with("### ") {
        Some(3)
    } else if line.starts_with("## ") {
        Some(2)
    } else if line.starts_with("# ") {
        Some(1)
    } else {
        None
    }
}

/// `#` 后跟哪个词表标签。`rest` 是 `#` 之后的文本，返回（种类，词长）。
/// 旧词（overdue/doing/blocked）沿用历史规则：前缀匹配即标签（`#doingx` 亦识别，
/// 不顺便重定义旧语法）；新词（paused/ts/t）要求词边界（行尾或空格），
/// 否则 `#today` 会被 `#t` 吞掉。
fn tag_kind_at(rest: &str) -> Option<(FlagOrBlocked, usize)> {
    let b = rest.as_bytes();
    let bounded = |len: usize| b.len() == len || b[len] == b' ';
    if rest.starts_with("overdue") {
        Some((FlagOrBlocked::Overdue, "overdue".len()))
    } else if rest.starts_with("doing") {
        Some((FlagOrBlocked::Doing, "doing".len()))
    } else if rest.starts_with("blocked") {
        Some((FlagOrBlocked::Blocked, "blocked".len()))
    } else if rest.starts_with("paused") && bounded("paused".len()) {
        Some((FlagOrBlocked::Paused, "paused".len()))
    } else if rest.starts_with("ts") && bounded("ts".len()) {
        Some((FlagOrBlocked::TimeStart, "ts".len()))
    } else if b.first() == Some(&b't') && bounded(1) {
        Some((FlagOrBlocked::Time, 1))
    } else {
        None
    }
}

/// 解析单个任务行。返回 None 表示不是任务行。
fn parse_task_line(line: &str) -> Option<ParsedLine> {
    if !is_task_start(line) {
        return None;
    }
    let b = line.as_bytes();
    let cb_span = (3usize, 4usize);
    let checked = b[3] == b'x';
    let mut pos = 5usize; // "] " 之后
    if pos < b.len() && b[pos] == b' ' {
        pos += 1;
    }

    // 优先级 (P0..P3)
    let mut priority = None;
    let mut prio_span = None;
    if b.len() >= pos + 4 && b[pos] == b'(' && b[pos + 1] == b'P' {
        let d = b[pos + 2];
        if (b'0'..=b'3').contains(&d) && b[pos + 3] == b')' {
            priority = Some(match d {
                b'0' => Priority::P0,
                b'1' => Priority::P1,
                b'2' => Priority::P2,
                _ => Priority::P3,
            });
            prio_span = Some((pos, pos + 4));
            pos += 4;
            if pos < b.len() && b[pos] == b' ' {
                pos += 1;
            }
        }
    }

    // 分类标签：封闭词表，紧跟前缀
    let mut cat_span = None;
    let mut category = Category::Uncategorized;
    for (word, cat) in [("[工作]", Category::Work), ("[个人]", Category::Personal)] {
        let wb = word.as_bytes();
        if b.len() >= pos + wb.len() && &b[pos..pos + wb.len()] == wb {
            cat_span = Some((pos, pos + wb.len()));
            category = cat;
            pos += wb.len();
            if pos < b.len() && b[pos] == b' ' {
                pos += 1;
            }
            break;
        }
    }

    // 内容区 + 尾部标签扫描
    let content_start = pos;
    let mut tags: Vec<TagSpan> = Vec::new();
    let mut content_end = b.len();
    let mut i = pos;
    let mut scanning_tags = false;
    while i < b.len() {
        if b[i] == b'#' && (i == 0 || b[i - 1] == b' ') {
            // 潜在标签：识别封闭词表
            let rest = &line[i + 1..];
            if let Some((kind, len)) = tag_kind_at(rest) {
                if !scanning_tags {
                    content_end = trim_end_at(line, i);
                    scanning_tags = true;
                }
                let tag_start = i;
                let tag_end = i + 1 + len;
                let mut reason = None;
                let mut value = None;
                let mut j = tag_end;
                if kind == FlagOrBlocked::Blocked {
                    // 原因词：直到下一个 " #" 或行尾。
                    // v0.8 边界修复：原因起点本身就是 '#'（如 `#blocked #doing`）时
                    // 原判断 `re > rs` 不成立会把后续标签吞进原因——`re == rs` 的
                    // 合法标签边界同样终止，且不再访问 re-1 字节。
                    let rs = skip_spaces(line, j);
                    let mut re = rs;
                    while re < b.len() {
                        if b[re] == b'#' && (re == rs || b[re - 1] == b' ') {
                            break;
                        }
                        re += 1;
                    }
                    let re_trim = trim_end_at(line, re);
                    if re_trim > rs {
                        reason = Some((rs, re_trim));
                        j = re_trim;
                    }
                } else if matches!(kind, FlagOrBlocked::Time | FlagOrBlocked::TimeStart) {
                    // 数值：与原因词同规则扫描（下一个 " #" 或行尾），非法文本照记，
                    // 由派生层标记 invalid、下次状态手术整体重写。
                    let vs = skip_spaces(line, j);
                    let mut ve = vs;
                    while ve < b.len() {
                        if b[ve] == b'#' && (ve == vs || b[ve - 1] == b' ') {
                            break;
                        }
                        ve += 1;
                    }
                    // 合法数字只占一个 token；其后正文是注记，不能吞入计时值。
                    // 非法值保留原有区间，供 invalid 派生及状态手术清理。
                    let first_end = line[vs..ve].find(' ').map_or(ve, |n| vs + n);
                    if line[vs..first_end].parse::<u64>().is_ok() {
                        ve = first_end;
                    }
                    let ve_trim = trim_end_at(line, ve);
                    if ve_trim > vs {
                        value = Some((vs, ve_trim));
                        j = ve_trim;
                    }
                }
                tags.push(TagSpan { kind, span: (tag_start, tag_end), reason, value });
                i = j;
                continue;
            }
        }
        i += 1;
    }

    let content_end = if scanning_tags { content_end } else { b.len() };
    // 前缀解析吃掉的分隔空格可能再被 trim_end_at 吃一遍（如 "- [ ] (P1) #doing"），
    // 起点越过终点的反向切片会 panic——release 是 panic=abort，整进程死。
    let content_start = content_start.min(content_end);
    let content = line[content_start..content_end].trim().to_string();

    // 注记：标签之后的非标签文本（如 "补记9/2"）
    let mut annotations: Vec<&str> = Vec::new();
    let mut cur = content_end;
    for t in &tags {
        let seg_end = t.span.0;
        if cur < seg_end {
            let seg = line[cur..seg_end].trim();
            if !seg.is_empty() {
                annotations.push(seg);
            }
        }
        // 游标须跨过标签的值区间：#t/#ts 的数值、#blocked 的原因词。
        // 只跳标签词会把 `#t 45` 的 45 漏成注记、拼进 display（v0.8 新标签引入的回归）。
        let mut tag_end = t.span.1;
        if let Some((_, ve)) = t.value {
            tag_end = tag_end.max(ve);
        }
        if let Some((_, re)) = t.reason {
            tag_end = tag_end.max(re);
        }
        cur = tag_end;
    }
    if cur < b.len() {
        let seg = line[cur..].trim();
        if !seg.is_empty() {
            annotations.push(seg);
        }
    }

    let doing = tags.iter().any(|t| t.kind == FlagOrBlocked::Doing);
    let overdue = tags.iter().any(|t| t.kind == FlagOrBlocked::Overdue);
    let paused = tags.iter().any(|t| t.kind == FlagOrBlocked::Paused);
    let blocked_reason = tags
        .iter()
        .find(|t| t.kind == FlagOrBlocked::Blocked)
        .and_then(|t| t.reason.map(|(s, e)| line[s..e].to_string()));

    // 计时派生：#t 取最后出现的值（非法→invalid、按 0；超界封顶不算非法），#ts 同理。
    let mut timer_base: Option<u64> = None;
    let mut timer_started_at: Option<u64> = None;
    let mut timer_invalid = false;
    for t in &tags {
        match t.kind {
            FlagOrBlocked::Time => {
                let raw = t.value.map(|(s, e)| &line[s..e]);
                match raw.and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) => timer_base = Some(n.min(TIMER_CAP_SECS)),
                    None => timer_invalid = true,
                }
            }
            FlagOrBlocked::TimeStart => {
                let raw = t.value.map(|(s, e)| &line[s..e]);
                match raw.and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) => timer_started_at = Some(n),
                    None => timer_invalid = true,
                }
            }
            _ => {}
        }
    }
    let timer_base = timer_base.unwrap_or(0);

    // 状态派生（见模块注释的优先级表）
    let has_timer_tags = tags.iter().any(|t| {
        matches!(t.kind, FlagOrBlocked::Time | FlagOrBlocked::TimeStart)
    });
    let status = if checked {
        TaskStatus::Done
    } else if paused {
        TaskStatus::Paused // doing+paused 并存显 Paused（人工合并文件的收敛方向）
    } else if doing {
        TaskStatus::Doing
    } else if has_timer_tags {
        TaskStatus::Paused // 仅有计时标签：非规范历史数据，按暂停解读（不丢累计）
    } else {
        TaskStatus::Todo
    };

    let mut display = content.clone();
    for a in annotations {
        display.push(' ');
        display.push_str(a);
    }

    Some(ParsedLine {
        checked,
        priority,
        category,
        content,
        display,
        doing,
        overdue,
        blocked_reason,
        status,
        timer_base,
        timer_started_at,
        timer_invalid,
        spans: Spans { cb: cb_span, prio: prio_span, cat: cat_span, content: (content_start, content_end), tags },
    })
}

fn skip_spaces(line: &str, mut i: usize) -> usize {
    let b = line.as_bytes();
    while i < b.len() && b[i] == b' ' {
        i += 1;
    }
    i
}

/// 剥掉一层结构缩进（写入侧统一两空格；历史 tab 行剥一个 tab）。
/// 单空格缩进或更深前缀不是结构缩进，剩余前导空白属于文本本身。
fn strip_structural_indent(line: &str) -> &str {
    if let Some(r) = line.strip_prefix("  ") {
        r
    } else if let Some(r) = line.strip_prefix('\t') {
        r
    } else {
        line
    }
}

/// 把 end 收敛到不包含尾随空格的位置（内容与标签间的分隔空格不属于内容）。
fn trim_end_at(line: &str, end: usize) -> usize {
    let b = line.as_bytes();
    let mut e = end.min(b.len());
    while e > 0 && b[e - 1] == b' ' {
        e -= 1;
    }
    e
}

/// 用户自由文本入库守卫（命令层调用）：
/// - 拒绝换行：markdown 按行组织，注入 \n 会伪造任务行/标题行
///   （多行编辑走 sub_lines 参数，不从这里放开）；
/// - 拒绝活标签词：与 tag_kind_at 相同的识别规则（行首/空格后跟
///   #overdue/#doing/#blocked/#paused/#t/#ts），否则用户文本下次解析
///   变成真状态标签，#overdue/#t 还无法从 UI 移除。
pub fn validate_user_text(text: &str) -> Result<(), String> {
    if text.contains(['\n', '\r']) {
        return Err("内容不能包含换行（换行请用 Shift+回车生成子行）".into());
    }
    let b = text.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'#'
            && (i == 0 || b[i - 1] == b' ')
            && tag_kind_at(&text[i + 1..]).is_some()
        {
            return Err(
                "内容不能包含 #overdue/#doing/#blocked/#paused/#t/#ts 状态标签".into(),
            );
        }
    }
    Ok(())
}

/// 子行文本入库守卫：只拦换行（与主行一致的行组织约束）；标签词在子行里
/// 是普通正文（缩进保护使其不会被解析成状态），照常保留。
pub fn validate_sub_lines(subs: &[String]) -> Result<(), String> {
    if subs.iter().any(|s| s.contains(['\n', '\r'])) {
        return Err("子行不能包含换行".into());
    }
    Ok(())
}

/// 撤销恢复的原始行：只拦换行（行内标签合法——本就是文件里的真实任务行）。
pub fn validate_restore_lines(lines: &[String]) -> Result<(), String> {
    if lines.iter().any(|l| l.contains(['\n', '\r'])) {
        return Err("恢复的行不能包含换行".into());
    }
    Ok(())
}

impl TodoFile {
    pub fn parse(text: &str) -> TodoFile {
        let crlf = text.contains("\r\n");
        let trailing_newline = text.is_empty() || text.ends_with('\n');
        let normalized = if crlf { text.replace("\r\n", "\n") } else { text.to_string() };
        let mut lines: Vec<String> = normalized.split('\n').map(|s| s.to_string()).collect();
        // "a\n" split => ["a", ""]：末尾空串代表换行符，收回
        if trailing_newline && lines.last().is_some_and(|s| s.is_empty()) {
            lines.pop();
        }

        let mut tasks: Vec<TaskView> = Vec::new();
        let mut parsed: Vec<ParsedLine> = Vec::new();
        let mut section = String::new();
        let mut subsection: Option<String> = None;
        let mut last_task: Option<usize> = None; // lines 下标

        for (idx, line) in lines.iter().enumerate() {
            if let Some(level) = is_heading(line) {
                let title = line.trim_start_matches('#').trim().to_string();
                if level <= 2 {
                    section = title;
                    subsection = None;
                } else {
                    subsection = Some(title);
                }
                last_task = None;
                continue;
            }
            if let Some(p) = parse_task_line(line) {
                let view = TaskView {
                    line_idx: idx,
                    checked: p.checked,
                    priority: p.priority,
                    category: p.category,
                    display: p.display.clone(),
                    content: p.content.clone(),
                    doing: p.doing,
                    overdue: p.overdue,
                    blocked_reason: p.blocked_reason.clone(),
                    status: p.status,
                    timer_secs: p.timer_base,
                    timer_started_at: p.timer_started_at,
                    timer_invalid: p.timer_invalid,
                    sub_lines: Vec::new(),
                    section: section.clone(),
                    subsection: subsection.clone(),
                };
                tasks.push(view);
                parsed.push(p);
                last_task = Some(idx);
            } else {
                // 缩进续行归属上一个任务（含缩进空白行——任务块边界 = 主行后
                // 连续缩进行，空白行属于块的内部结构，删除/替换子行时一并处理）。
                // sub_lines 只剥结构缩进（写入侧统一两空格），其余前导空白原样保留。
                let is_indented = line.starts_with(' ') || line.starts_with('\t');
                if is_indented {
                    if let Some(t) = last_task {
                        let view = tasks.last_mut().unwrap();
                        if view.line_idx == t {
                            view.sub_lines.push(strip_structural_indent(line).to_string());
                        }
                    }
                } else {
                    last_task = None;
                }
            }
        }

        TodoFile { lines, crlf, trailing_newline, tasks, parsed }
    }

    /// 序列化。未做任何编辑时与原文字节一致。
    pub fn serialize(&self) -> String {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self.lines.join(eol);
        if self.trailing_newline {
            out.push_str(eol);
        }
        out
    }

    fn task_by_line(&self, line_idx: usize) -> Option<usize> {
        self.tasks.iter().position(|t| t.line_idx == line_idx)
    }

    /// 查询任务当前派生态（旧命令的状态机适配入口）。
    pub fn status_at(&self, line_idx: usize) -> Result<TaskStatus, String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        Ok(self.parsed[slot].status)
    }

    /// 手术后：用新行重解析该任务，同步派生视图与缓存。
    fn refresh(&mut self, slot: usize) {
        let new_line = self.lines[self.tasks[slot].line_idx].clone();
        match parse_task_line(&new_line) {
            Some(p) => {
                let v = &mut self.tasks[slot];
                v.checked = p.checked;
                v.priority = p.priority;
                v.category = p.category;
                v.display = p.display.clone();
                v.content = p.content.clone();
                v.doing = p.doing;
                v.overdue = p.overdue;
                v.blocked_reason = p.blocked_reason.clone();
                v.status = p.status;
                v.timer_secs = p.timer_base;
                v.timer_started_at = p.timer_started_at;
                v.timer_invalid = p.timer_invalid;
                self.parsed[slot] = p;
            }
            None => { /* 行不再是任务：保守保留旧视图，调用方不应制造这种编辑 */ }
        }
    }

    /// 勾选翻转。只改 [ ] ↔ [x] 的一个字符。
    pub fn set_checked(&mut self, line_idx: usize, checked: bool) -> Result<(), String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let (s, e) = self.parsed[slot].spans.cb;
        let line = self.lines[line_idx].clone();
        let ch = if checked { "x" } else { " " };
        let bytes = line.as_bytes();
        if s >= e || e > bytes.len() {
            return Err("checkbox 区间非法".into());
        }
        let mut new_line = String::with_capacity(line.len());
        new_line.push_str(&line[..s]);
        new_line.push_str(ch);
        new_line.push_str(&line[e..]);
        self.lines[line_idx] = new_line;
        self.refresh(slot);
        Ok(())
    }

    /// 设置分类标签：Some=插入/替换 `[工作]`/`[个人]`，None=移除。
    pub fn set_category(&mut self, line_idx: usize, cat: Option<Category>) -> Result<(), String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let old = self.parsed[slot].spans.cat;
        let line = self.lines[line_idx].clone();
        let mut new_line = String::with_capacity(line.len() + 8);
        match (old, cat) {
            (None, None) => return Ok(()),
            (Some((s, e)), None) => {
                // 移除标签 + 其后一个空格（若有）
                let mut cut = e;
                let b = line.as_bytes();
                if cut < b.len() && b[cut] == b' ' {
                    cut += 1;
                }
                new_line.push_str(&line[..s]);
                new_line.push_str(&line[cut..]);
            }
            (None, Some(c)) => {
                // 插入位置：内容区起点（即优先级/checkbox 前缀之后）
                let anchor = self.parsed[slot].spans.content.0;
                let word = match c {
                    Category::Work => "[工作] ",
                    _ => "[个人] ",
                };
                new_line.push_str(&line[..anchor]);
                new_line.push_str(word);
                new_line.push_str(&line[anchor..]);
            }
            (Some((s, e)), Some(c)) => {
                let word = match c {
                    Category::Work => "[工作]",
                    _ => "[个人]",
                };
                new_line.push_str(&line[..s]);
                new_line.push_str(word);
                new_line.push_str(&line[e..]);
            }
        }
        if new_line != line {
            self.lines[line_idx] = new_line;
            self.refresh(slot);
        }
        Ok(())
    }

    /// 设置优先级：Some=插入/替换 `(P1)`，None=移除。优先级位于 checkbox 后、分类前。
    pub fn set_priority(&mut self, line_idx: usize, prio: Option<Priority>) -> Result<(), String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let old = self.parsed[slot].spans.prio;
        let line = self.lines[line_idx].clone();
        let mut new_line = String::with_capacity(line.len() + 5);
        match (old, prio) {
            (None, None) => return Ok(()),
            (Some((s, e)), None) => {
                // 移除优先级 + 其后一个空格（若有）
                let mut cut = e;
                let b = line.as_bytes();
                if cut < b.len() && b[cut] == b' ' {
                    cut += 1;
                }
                new_line.push_str(&line[..s]);
                new_line.push_str(&line[cut..]);
            }
            (None, Some(p)) => {
                // 插入位置：分类标签或内容区的起点（优先级永远在最前缀）
                let anchor = self.parsed[slot]
                    .spans
                    .cat
                    .map(|(s, _)| s)
                    .unwrap_or(self.parsed[slot].spans.content.0);
                new_line.push_str(&line[..anchor]);
                new_line.push('(');
                new_line.push_str(p.as_str());
                new_line.push_str(") ");
                new_line.push_str(&line[anchor..]);
            }
            (Some((s, e)), Some(p)) => {
                // 原地替换 `(P0)` ↔ `(P2)`
                new_line.push_str(&line[..s]);
                new_line.push('(');
                new_line.push_str(p.as_str());
                new_line.push(')');
                new_line.push_str(&line[e..]);
            }
        }
        if new_line != line {
            self.lines[line_idx] = new_line;
            self.refresh(slot);
        }
        Ok(())
    }

    /// 追加状态标签（#overdue / #doing）。
    pub fn add_flag(&mut self, line_idx: usize, flag: Flag) -> Result<(), String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        if match flag {
            Flag::Overdue => self.parsed[slot].overdue,
            Flag::Doing => self.parsed[slot].doing,
        } {
            return Ok(()); // 幂等
        }
        let word = match flag {
            Flag::Overdue => " #overdue",
            Flag::Doing => " #doing",
        };
        let mut new_line = self.lines[line_idx].clone();
        new_line.push_str(word);
        self.lines[line_idx] = new_line;
        self.refresh(slot);
        Ok(())
    }

    /// 移除状态标签（同词重复出现的历史行全部清除——v0.8 修复原「只删首个」）。
    /// #blocked 连带原因词一起移除（原因属于标签）。
    pub fn remove_flag(&mut self, line_idx: usize, flag: Flag) -> Result<(), String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let line = self.lines[line_idx].clone();
        let b = line.as_bytes();
        // 收集全部匹配区间（各含一个前导空格），倒序删除避免位移
        let mut cuts: Vec<(usize, usize)> = Vec::new();
        for t in &self.parsed[slot].spans.tags {
            let hit = match flag {
                Flag::Overdue => t.kind == FlagOrBlocked::Overdue,
                Flag::Doing => t.kind == FlagOrBlocked::Doing,
            };
            if !hit {
                continue;
            }
            let (s, e) = t.span;
            let mut start = s;
            if start > 0 && b[start - 1] == b' ' {
                start -= 1;
            }
            let mut end = e;
            if let Some((_, re)) = t.reason {
                end = end.max(re);
            }
            cuts.push((start, end));
        }
        if cuts.is_empty() {
            return Ok(());
        }
        cuts.sort_unstable();
        let mut new_line = String::with_capacity(line.len());
        let mut cursor = 0usize;
        for (s, e) in cuts {
            new_line.push_str(&line[cursor..s]);
            cursor = e;
        }
        new_line.push_str(&line[cursor..]);
        self.lines[line_idx] = new_line;
        self.refresh(slot);
        Ok(())
    }

    /// v0.8 状态手术：把任务切到 target（todo 由命令层拒绝），now 为服务端 unix 秒
    /// （整次手术只由调用方读一次时钟）。只动 checkbox、状态标签与计时标签的
    /// 字节区间；`#overdue`/`#blocked`/注记原样保留，重复状态 token 全部清理：
    /// - 结算：Doing → Paused/Done 时 `t = min(t + max(0, now-ts), CAP)`、删 ts
    ///   （ts 缺失/非法则不加，时钟倒拨由 saturating_sub 保证 delta≥0）；
    /// - 进行：任何 → Doing 保留 t、写 `ts=now`（幂等 Doing 不重设；遗留无 ts 补启）；
    /// - 复活：Done → Doing 保留 t 冻结值从 now 续计；Done → Paused 保留冻结值停暂停
    ///   （v0.8.1 取消勾选复活即停暂停，点状态钮才开始续计）；
    /// - 一旦离开待开始必有 `#t`（零值不可省略）；Done 残留 ts 允许（幂等不清理）。
    pub fn set_status_at(&mut self, line_idx: usize, target: TaskStatus, now: u64) -> Result<(), String> {
        if target == TaskStatus::Todo {
            return Err("待开始是一次性初始状态，不可恢复".into());
        }
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let cur = self.parsed[slot].status;
        if cur == target
            && !(target == TaskStatus::Doing && self.parsed[slot].timer_started_at.is_none())
        {
            return Ok(()); // 同态幂等；唯一例外：Doing 遗留无 ts 走下面补启
        }

        let line = self.lines[line_idx].clone();
        let b = line.as_bytes();

        // 结算值：base 为已落盘累计（派生时封顶/非法归零），Doing 才计增量
        let base = self.parsed[slot].timer_base;
        let delta = if cur == TaskStatus::Doing {
            self.parsed[slot]
                .timer_started_at
                .map(|ts| now.saturating_sub(ts))
                .unwrap_or(0)
        } else {
            0
        };
        let settled = base.saturating_add(delta).min(TIMER_CAP_SECS);

        // 所有编辑都使用原始行区间；删除状态标签后不重扫中间串。
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        let mut seen_t = false;
        let mut replaced_t = false;
        for t in &self.parsed[slot].spans.tags {
            let removable = match t.kind {
                FlagOrBlocked::Doing | FlagOrBlocked::Paused | FlagOrBlocked::TimeStart => true,
                FlagOrBlocked::Time => {
                    let first = !seen_t;
                    seen_t = true;
                    if first {
                        if let Some((vs, ve)) = t.value {
                            edits.push((vs, ve, settled.to_string()));
                            replaced_t = true;
                            continue;
                        }
                    }
                    true // 重复或裸 #t 删除；裸标签在行尾补写带空格的数值
                }
                _ => false,
            };
            if removable {
                let mut start = t.span.0;
                if start > 0 && b[start - 1] == b' ' {
                    start -= 1;
                }
                let end = t.value.map_or(t.span.1, |(_, ve)| ve.max(t.span.1));
                edits.push((start, end, String::new()));
            }
        }
        edits.sort_unstable_by_key(|edit| edit.0);
        let mut out = String::with_capacity(line.len() + 32);
        let mut cursor = 0;
        for (start, end, replacement) in edits {
            out.push_str(&line[cursor..start]);
            out.push_str(&replacement);
            cursor = end;
        }
        out.push_str(&line[cursor..]);
        if !replaced_t {
            out.push_str(&format!(" #t {settled}"));
        }
        match target {
            TaskStatus::Doing => {
                out.push_str(" #doing");
                out.push_str(&format!(" #ts {now}"));
            }
            TaskStatus::Paused => out.push_str(" #paused"),
            TaskStatus::Done => {}
            TaskStatus::Todo => unreachable!("入口已拒绝"),
        }

        // checkbox：Done→x；Doing/Paused→空格（复活路径）。前缀未被上述手术触碰。
        let (cs, ce) = self.parsed[slot].spans.cb;
        if out.len() <= ce {
            return Err("checkbox 区间非法".into());
        }
        out.replace_range(cs..ce, if target == TaskStatus::Done { "x" } else { " " });

        self.lines[line_idx] = out;
        self.refresh(slot);
        Ok(())
    }

    /// 替换内容主体（保留前缀、标签、注记）。
    pub fn set_content(&mut self, line_idx: usize, new_content: &str) -> Result<(), String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let (s, e) = self.parsed[slot].spans.content;
        let line = self.lines[line_idx].clone();
        let mut new_line = String::with_capacity(line.len());
        new_line.push_str(&line[..s]);
        new_line.push_str(new_content);
        new_line.push_str(&line[e..]);
        self.lines[line_idx] = new_line;
        self.refresh(slot);
        Ok(())
    }

    /// 设置/清除 `#blocked`（PRD F7：blocked 可写且带原因文本）。
    /// Some → 行尾写 ` #blocked 原因`（已有 blocked 先移除再写，原因更新）；
    /// None → 移除标签连带原因词（原因属于标签）。
    pub fn set_blocked(&mut self, line_idx: usize, reason: Option<&str>) -> Result<(), String> {
        let slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let line = self.lines[line_idx].clone();
        let b = line.as_bytes();

        // 移除既有 #blocked（含前导空格与原因词；原因词区间解析时已收口）
        let mut new_line = line.clone();
        if let Some(tag) = self.parsed[slot].spans.tags.iter().find(|t| t.kind == FlagOrBlocked::Blocked) {
            let mut end = tag.span.1;
            if let Some((_, re)) = tag.reason {
                end = end.max(re);
            }
            let mut start = tag.span.0;
            if start > 0 && b[start - 1] == b' ' {
                start -= 1;
            }
            new_line = format!("{}{}", &line[..start], &line[end..]);
        }

        if let Some(r) = reason {
            let r = r.trim();
            if r.is_empty() {
                return Err("blocked 原因不能为空白".into());
            }
            new_line.push_str(&format!(" #blocked {r}"));
        }
        self.lines[line_idx] = new_line;
        // 重建该行解析缓存（行内区间全部变化）
        let text = self.serialize();
        *self = TodoFile::parse(&text);
        Ok(())
    }

    /// 任务块末尾（不含）：主行 + 其后连续缩进行（含缩进空白行，v0.8 块边界）。
    pub fn task_block_end(&self, line_idx: usize) -> usize {
        let mut end = line_idx + 1;
        while end < self.lines.len() {
            let l = &self.lines[end];
            if l.starts_with(' ') || l.starts_with('\t') {
                end += 1;
            } else {
                break;
            }
        }
        end
    }

    /// 删除任务行（连同其缩进子行），返回被删原始行供撤销恢复。
    /// 块边界与 parse 的归属规则一致：主行后连续缩进行，含缩进空白行。
    pub fn delete_task(&mut self, line_idx: usize) -> Result<Vec<String>, String> {
        let _slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let end = self.task_block_end(line_idx);
        let removed: Vec<String> = self.lines.drain(line_idx..end).collect();
        // 重建派生视图（行号整体变化）
        let text = self.serialize();
        *self = TodoFile::parse(&text);
        Ok(removed)
    }

    /// 整块替换任务的子行。subs 每条写入为「两空格 + 文本」（结构缩进协议）；
    /// 空串写入为单独的缩进空白行（保持块内空行）。行号变化，全量重建。
    pub fn replace_sub_lines(&mut self, line_idx: usize, subs: &[String]) -> Result<(), String> {
        let _slot = self.task_by_line(line_idx).ok_or("line 不是任务行")?;
        let end = self.task_block_end(line_idx);
        let new_lines: Vec<String> = subs.iter().map(|s| format!("  {s}")).collect();
        self.lines.splice(line_idx + 1..end, new_lines);
        let text = self.serialize();
        *self = TodoFile::parse(&text);
        Ok(())
    }

    /// 在 line_idx 处原样插回若干行（撤销删除；行内容不做任何改写）。
    pub fn insert_lines_at(&mut self, line_idx: usize, removed: Vec<String>) {
        let at = line_idx.min(self.lines.len());
        for (i, l) in removed.into_iter().enumerate() {
            self.lines.insert(at + i, l);
        }
        let text = self.serialize();
        *self = TodoFile::parse(&text);
    }

    /// 定位指定二级段「平铺任务区」末尾的插入行号。
    /// 平铺区 = 段落标题（含紧随的注释行）之后、第一个三级子区/下一二级段/EOF 之前；
    /// 插入点 = 区内最后一个非空行之后。add_task 与跨日流转的整块搬运共用。
    pub fn section_flat_insert_at(&self, section: &str) -> Result<usize, String> {
        let sec_lower = section.to_lowercase();
        let mut sec_range: Option<(usize, usize)> = None;
        let mut i = 0usize;
        let mut cur: Option<(usize, usize)> = None;
        while i < self.lines.len() {
            if let Some(level) = is_heading(&self.lines[i]) {
                if level <= 2 {
                    // 收口：段终点 = 当前标题行（占位值 lines.len() 作废）
                    if let Some((s, _)) = cur.take() {
                        sec_range = Some((s, i));
                    }
                    let title = self.lines[i].trim_start_matches('#').trim().to_string();
                    if title.to_lowercase() == sec_lower {
                        cur = Some((i + 1, self.lines.len()));
                    }
                } else if cur.is_some() {
                    let (s, _) = cur.unwrap();
                    cur = Some((s, i));
                    // 继续找段尾由下一个 ## 或 EOF 决定；这里直接封口
                    sec_range = cur;
                    cur = None;
                    i += 1;
                    continue;
                }
            }
            i += 1;
        }
        if let Some(r) = cur {
            sec_range = Some(r);
        }
        let (sec_start, sec_end) = sec_range.ok_or(format!("未找到段落 ## {}", section))?;

        // 在平铺区内找最后一个非空行
        let mut insert_at = sec_start;
        let mut j = sec_start;
        while j < sec_end {
            if !self.lines[j].trim().is_empty() {
                insert_at = j + 1;
            }
            j += 1;
        }
        Ok(insert_at)
    }

    /// 在指定二级段的「平铺任务区」末尾追加新任务（可选子行，两空格结构缩进）。
    /// 返回主行行号。
    pub fn add_task(
        &mut self,
        section: &str,
        category: Category,
        priority: Priority,
        text: &str,
        subs: &[String],
    ) -> Result<usize, String> {
        let insert_at = self.section_flat_insert_at(section)?;

        let cat_word = match category {
            Category::Work => " [工作] ",
            Category::Personal => " [个人] ",
            Category::Uncategorized => " ",
        };
        let mut block = vec![format!("- [ ] ({}){}{}", priority.as_str(), cat_word, text)];
        block.extend(subs.iter().map(|s| format!("  {s}")));
        self.lines.splice(insert_at..insert_at, block);
        let text = self.serialize();
        *self = TodoFile::parse(&text);
        Ok(insert_at)
    }
}

/// 冲突自动合并（v0.8.6）的行级结果：合并后的原始行 + 裁决注记（进 meta.json）。
/// 任务身份键：内容主体（merge_resolve 三方配对用；不含优先级/分类——
/// 那些是可仲裁属性，进键会让单侧调档拆成两个任务、丢掉对端勾选）。
pub(crate) fn task_identity(line: &str) -> Option<String> {
    parse_task_line(line).map(|p| p.content)
}

pub(crate) struct MergedTaskLine {
    pub line: String,
    pub notes: Vec<String>,
}

/// 任务行三方语义合并。三方须为「同一任务」的三个版本（调用方 merge_resolve
/// 按身份键＝内容＋子行配对，见 merge_resolve 模块注释）。
///
/// 快路径：任两方字节相等 → 直接取第三方原文（零手术、字节保真）。
/// 双改路径以 ours 原始行为底做字节区间手术（保 #overdue/#blocked 原因与
/// 人工注记的原文），属性裁决全部朝「不丢」方向：
/// - checked 或运算；状态三方裁决（单侧改动取动侧，双改取更高进度
///   Done > Doing > Paused > Todo，doing 优先于 paused）
/// - `#t` 取 max（TIMER_CAP 已在派生时封顶）；`#ts` 仅 Doing 保留（ours 优先）
/// - 优先级/分类标准三方，双改不同取 ours + note
/// - 内容主体（content）双改且不同 → None（真冲突，交人工）
/// - theirs 独有的用户文本（注记尾段、受阻原因分歧）并入行尾，不丢记录
pub(crate) fn three_way_merge_task_line(
    base: Option<&str>,
    ours: &str,
    theirs: &str,
) -> Option<MergedTaskLine> {
    // 快路径：字节比较零成本且免解析
    if ours == theirs {
        return Some(MergedTaskLine { line: ours.to_string(), notes: Vec::new() });
    }
    let bp = match base {
        None => None, // add/add：双端各自新建的同键任务，无 base 参照
        Some(b) => {
            if b == theirs {
                return Some(MergedTaskLine { line: ours.to_string(), notes: Vec::new() });
            }
            if b == ours {
                return Some(MergedTaskLine { line: theirs.to_string(), notes: Vec::new() });
            }
            Some(parse_task_line(b)?) // base 行不是任务行 → 无法配对
        }
    };

    let op = parse_task_line(ours)?;
    let tp = parse_task_line(theirs)?;
    // 内容主体双改且不同：同一任务的文案被两端改成两个方向 → 人工
    if op.content != tp.content {
        if bp.as_ref().is_some_and(|bp| bp.content == op.content) {
            return Some(MergedTaskLine { line: theirs.to_string(), notes: Vec::new() });
        }
        if bp.as_ref().is_some_and(|bp| bp.content == tp.content) {
            return Some(MergedTaskLine { line: ours.to_string(), notes: Vec::new() });
        }
        return None;
    }

    let mut notes: Vec<String> = Vec::new();

    // ── 目标属性裁决 ──────────────────────────────────────────────
    // checked 标准三方：单侧改动取动侧（本机复活 Done→Paused 不会被对端的
    // 陈旧勾选翻回），双改取或运算
    let checked = match bp.as_ref() {
        Some(bp) if tp.checked == bp.checked => op.checked,
        Some(bp) if op.checked == bp.checked => tp.checked,
        _ => op.checked || tp.checked,
    };
    let status = match bp.as_ref() {
        Some(bp) => three_way_status(bp.status, op.status, tp.status),
        None => max_status(op.status, tp.status),
    };
    let timer = op.timer_base.max(tp.timer_base);
    let ts = if status == TaskStatus::Doing {
        op.timer_started_at.or(tp.timer_started_at)
    } else {
        None
    };
    if op.timer_invalid || tp.timer_invalid {
        notes.push("计时标签存在非法值，按现值取 max 合并".into());
    }
    // 优先级/分类：标准三方（单侧改动取动侧，双改取 ours）
    let prio = match (bp.as_ref().and_then(|bp| bp.priority), op.priority, tp.priority) {
        (_, o, t) if o == t => o,
        (b, _, t) if b == t => op.priority, // ours 是动侧
        (b, o, t) if b == o => t,           // theirs 是动侧
        _ => {
            notes.push(format!(
                "优先级双端改动不同（本机 {:?} / 对端 {:?}），取本机",
                op.priority, tp.priority
            ));
            op.priority
        }
    };
    let cat = match (
        bp.as_ref().map(|bp| bp.category).unwrap_or(Category::Uncategorized),
        op.category,
        tp.category,
    ) {
        (_, o, t) if o == t => o,
        (b, _, t) if b == t => op.category,
        (b, o, t) if b == o => t,
        _ => {
            notes.push("分类双端改动不同，取本机".into());
            op.category
        }
    };

    // ── 以 ours 原始行为底的区间手术 ─────────────────────────────
    let b = ours.as_bytes();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    // 1) 状态标签手术（#doing/#paused/#t/#ts；重复 token 全清，手法同 set_status_at）。
    //    #overdue/#blocked 与注记文本区间保留原文。
    // 2) 追加段 tail：#t（离开 Todo 即必有）+ 状态标签 + theirs 加侧胜标签。
    //    tail 在 ours 的首个状态标签处「原位替换」而非行尾追加——注记只在标签
    //    之后的空隙存在，行尾追加会把注记挤到新标签之前、被 content 吞掉
    //    （identity 键漂移）。无状态标签可替换时才退化为行尾追加。
    let mut tail = String::new();
    if timer > 0 {
        // 最小形式：#t 0 是语义空值，不落盘（仅计时标签的历史行按 Paused
        // 解读，#paused 标签本身已足以派生同态）
        tail.push_str(&format!(" #t {timer}"));
    }
    match status {
        TaskStatus::Doing => {
            tail.push_str(" #doing");
            if let Some(ts) = ts {
                tail.push_str(&format!(" #ts {ts}"));
            }
        }
        TaskStatus::Paused => tail.push_str(" #paused"),
        TaskStatus::Done => {}
        TaskStatus::Todo => {}
    }
    // 标签加侧胜（对端新增且 base 没有）才补写；base 有而本机已清除的，
    // 不因对端陈旧副本而复活（单侧删除应生效）
    let base_overdue = bp.as_ref().is_some_and(|bp| bp.overdue);
    if !op.overdue && tp.overdue && !base_overdue {
        tail.push_str(" #overdue");
    }
    if op.overdue && !tp.overdue && base_overdue {
        notes.push("对端已移除 #overdue，本机保留".into());
    }

    // theirs 用户文本无损写入：注记尾段与受阻原因分歧落到行尾（写进
    // 仓库文件本身；备份/meta.json 会被修剪，不能作为用户文本的归宿）。
    // 追加一律以 " #" 起头：#blocked 原因词的区间贪婪扫到下一个 " #" 或
    // 行尾，直接拼在行尾会被下次解析吞进受阻原因。
    let mut extra = String::new();
    let ann_of = |p: &ParsedLine| {
        p.display.strip_prefix(&p.content).unwrap_or("").trim().to_string()
    };
    let (o_ann, t_ann) = (ann_of(&op), ann_of(&tp));
    if t_ann != o_ann && !t_ann.is_empty() {
        extra.push_str(&format!(" # {t_ann}"));
        notes.push(format!("对端注记「{t_ann}」已并入行尾"));
    }
    let base_blocked = bp.as_ref().and_then(|bp| bp.blocked_reason.clone());
    match (op.blocked_reason.as_deref(), tp.blocked_reason.as_deref()) {
        (None, Some(r)) if base_blocked.is_none() => {
            tail.push_str(&format!(" #blocked {r}"));
        }
        (Some(_), Some(t)) if Some(t) != base_blocked.as_deref() => {
            extra.push_str(&format!(" # 对端受阻原因：{t}"));
            notes.push(format!("#blocked 原因双端不同，取本机；对端原因「{t}」已并入行尾注记"));
        }
        // 本机已解除阻塞（base 有原因）、对端改了原因：解除生效不复活标签，
        // 对端文本不丢，落行尾注记
        (None, Some(r)) => {
            extra.push_str(&format!(" # 对端受阻原因：{r}"));
            notes.push(format!(
                "#blocked 本机已解除、对端仍受阻（原因「{r}」已并入行尾注记）"
            ));
        }
        _ => {}
    }

    let mut tail_placed = false;
    for t in &op.spans.tags {
        if !matches!(
            t.kind,
            FlagOrBlocked::Doing | FlagOrBlocked::Paused | FlagOrBlocked::Time | FlagOrBlocked::TimeStart
        ) {
            continue;
        }
        let mut start = t.span.0;
        if start > 0 && b[start - 1] == b' ' {
            start -= 1;
        }
        let end = t.value.map_or(t.span.1, |(_, ve)| ve.max(t.span.1));
        let replacement = if tail_placed {
            String::new()
        } else {
            tail_placed = true;
            tail.clone()
        };
        edits.push((start, end, replacement));
    }
    if !tail_placed && !tail.is_empty() {
        edits.push((b.len(), b.len(), tail));
    }
    if !extra.is_empty() {
        // 行尾并入 theirs 用户文本（注记/受阻原因分歧）；零长区间在行尾
        // 永不与其他编辑重叠，稳定排序保证落在 tail 之后
        edits.push((b.len(), b.len(), extra));
    }

    // 3) checkbox（区间在行首，不受尾部编辑影响）
    let (cs, ce) = op.spans.cb;
    edits.push((cs, ce, if checked { "x".to_string() } else { " ".to_string() }));

    // 4) 优先级/分类（裁决结果 != ours 当前值时才动区间；手法对齐
    //    set_priority/set_category：删除连尾随空格、插入带尾随空格）
    if prio != op.priority {
        let anchor = op.spans.cat.map(|(s, _)| s).unwrap_or(op.spans.content.0);
        match (op.spans.prio, prio) {
            (None, None) => {}
            (None, Some(p)) => edits.push((anchor, anchor, format!("({}) ", p.as_str()))),
            (Some((s, e)), None) => edits.push((s, cut_with_space(b, e), String::new())),
            (Some((s, e)), Some(p)) => edits.push((s, e, format!("({})", p.as_str()))),
        }
    }
    if cat != op.category {
        let anchor = op.spans.content.0;
        let word = |c: Category| match c {
            Category::Work => "[工作]",
            Category::Personal => "[个人]",
            Category::Uncategorized => "",
        };
        match (op.spans.cat, cat) {
            (None, c) if !word(c).is_empty() => edits.push((anchor, anchor, format!("{} ", word(c)))),
            (Some((s, e)), Category::Uncategorized) => {
                edits.push((s, cut_with_space(b, e), String::new()))
            }
            (Some((s, e)), c) => edits.push((s, e, word(c).to_string())),
            _ => {}
        }
    }

    // 区间全部基于 ours 原始行，排序后一次应用（稳定排序保追加序）
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(ours.len() + 32);
    let mut cursor = 0usize;
    for (start, end, replacement) in &edits {
        if *start < cursor {
            return None; // 区间重叠＝内部错误，拒绝产出
        }
        out.push_str(&ours[cursor..*start]);
        out.push_str(replacement);
        cursor = *end;
    }
    out.push_str(&ours[cursor..]);

    // 5) theirs 独有的 display 信息已由 extra 并入行尾（见上），无遗留处理

    // 产物必须仍是合法任务行且派生态符合裁决（防御）
    let check = parse_task_line(&out)?;
    if check.checked != checked || check.status != status || check.content != op.content {
        return None;
    }
    Some(MergedTaskLine { line: out, notes })
}

/// 状态三方裁决：单侧改动取动侧；双端都改（或无 base 参照）取更高进度。
/// 进度序 Done > Doing > Paused > Todo（计划：doing 优先于 paused）。
fn three_way_status(b: TaskStatus, o: TaskStatus, t: TaskStatus) -> TaskStatus {
    if t == b {
        return o;
    }
    if o == b {
        return t;
    }
    max_status(o, t)
}

/// 无 base 参照（add/add）时取更高进度。
fn max_status(o: TaskStatus, t: TaskStatus) -> TaskStatus {
    let rank = |s: TaskStatus| match s {
        TaskStatus::Todo => 0,
        TaskStatus::Paused => 1,
        TaskStatus::Doing => 2,
        TaskStatus::Done => 3,
    };
    if rank(o) >= rank(t) { o } else { t }
}

/// 删除标签/前缀时连同其一个尾随空格（对齐 set_category 的删除手法）。
fn cut_with_space(b: &[u8], end: usize) -> usize {
    if end < b.len() && b[end] == b' ' { end + 1 } else { end }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_absent_category_preserves_line_without_refresh() {
        let mut f = TodoFile::parse("- [ ] 任务A\n");
        let content_ptr = f.tasks[0].content.as_ptr();
        f.set_category(0, None).unwrap();
        assert_eq!(f.lines[0], "- [ ] 任务A");
        assert_eq!(f.tasks[0].content.as_ptr(), content_ptr);
    }

    #[test]
    fn clearing_absent_priority_preserves_line_without_refresh() {
        let mut f = TodoFile::parse("- [ ] 任务A\n");
        let content_ptr = f.tasks[0].content.as_ptr();
        f.set_priority(0, None).unwrap();
        assert_eq!(f.lines[0], "- [ ] 任务A");
        assert_eq!(f.tasks[0].content.as_ptr(), content_ptr);
    }

    /// QA 种子形态：子行带「  - 」两空格缩进 + dash 前缀（seed_v08.py 写法）。
    #[test]
    fn sub_lines_attach_with_dash_prefix() {
        let text = "# 2026-09-20 (周日)\n\n> 本日重点:\n\n## 日任务\n\
                    - [ ] (P1) [工作] 四态循环样本\n\
                    - [ ] (P2) [个人] 多行编辑样本\n  - 子行一：原备注甲\n  - 子行二：原备注乙\n  - 子行三：将被删除\n\
                    - [ ] (P3) [个人] 删除整块样本 #t 45 #doing\n  - 随块删除的子行一\n";
        let f = TodoFile::parse(text);
        assert_eq!(f.tasks.len(), 3);
        assert_eq!(f.tasks[1].content, "多行编辑样本");
        assert_eq!(
            f.tasks[1].sub_lines,
            vec!["- 子行一：原备注甲", "- 子行二：原备注乙", "- 子行三：将被删除"]
        );
        assert_eq!(f.tasks[2].sub_lines, vec!["- 随块删除的子行一"]);
    }

    /// 应用写回形态：子行两空格缩进、无 dash（store 写侧 format!("  {s}")）。
    #[test]
    fn sub_lines_attach_without_dash() {
        let text = "## 日任务\n\
                    - [ ] (P2) [个人] 多行编辑样本\n  子行一：原备注甲\n  子行二：原备注乙\n";
        let f = TodoFile::parse(text);
        assert_eq!(f.tasks.len(), 1);
        assert_eq!(f.tasks[0].sub_lines, vec!["子行一：原备注甲", "子行二：原备注乙"]);
    }

    /// 外部手写裸 `#t`（无值，UI 不可达）：结算须剥除后追加 ` #t N`——
    /// 原地粘数会成 `#t3600`，下次按整词扫描读作未知标签，计时归零。
    /// 结算值是派生量：裸 #t 读侧计 0，delta = now - ts = 3660 - 60 = 3600。
    #[test]
    fn bare_time_tag_settles_with_spaced_value() {
        let mut f = TodoFile::parse("## 日任务\n- [ ] (P1) [工作] 裸标签样本 #doing #ts 60 #t\n");
        f.set_status_at(1, TaskStatus::Done, 3660).unwrap();
        assert_eq!(f.lines[1], "- [x] (P1) [工作] 裸标签样本 #t 3600");
        // 回读仍是带值 #t（裸 #t 读侧按非法计 0，此处应为结算值）
        let reparsed = TodoFile::parse("## 日任务\n- [x] (P1) [工作] 裸标签样本 #t 3600\n");
        assert_eq!(reparsed.tasks[0].timer_secs, 3600);
        assert_eq!(reparsed.tasks[0].status, TaskStatus::Done);
    }

    /// 裸 `#t` 叠加 `#doing` 转 Paused：状态标签剥除 + 计时落带空格值，一次手术完成。
    #[test]
    fn bare_time_tag_with_doing_settles_to_paused() {
        let mut f = TodoFile::parse("## 日任务\n- [ ] (P1) [工作] 裸标签续跑 #doing #t\n");
        f.set_status_at(1, TaskStatus::Paused, 500).unwrap();
        assert_eq!(f.lines[1], "- [ ] (P1) [工作] 裸标签续跑 #t 0 #paused");
    }

    /// 标签值区间不得漏成注记：`#t 45` 的 45、`#ts` 的时间戳不进 display；
    /// 合法数值后的正文是注记；非法值仍扫描至行尾/下一 # 标签。
    /// （v0.8 回归：注记游标只跳标签词不跳数值，display 会拼成「任务 45」。）
    #[test]
    fn tag_values_do_not_leak_into_display() {
        let f = TodoFile::parse(
            "## 日任务\n- [ ] (P1) [工作] 计时任务 #t 45 #doing #ts 1789000000\n\
                      - [ ] (P2) [个人] 带真注记 #doing 补记9/2 #t 300\n",
        );
        assert_eq!(f.tasks[0].display, "计时任务");
        assert_eq!(f.tasks[1].display, "带真注记 补记9/2");
    }

    /// v0.8.1 复活语义：取消完成勾选（UI check 分支 target=paused）→ 复选框翻回、
    /// #t 冻结值保留、落 #paused 无 #ts（点状态钮才开始续计，不再直接回 doing）。
    /// done 行无 #ts → delta=0，settled 即原 #t 终值。
    #[test]
    fn done_revives_to_paused_frozen() {
        let mut f = TodoFile::parse("## 日任务\n- [x] (P1) [工作] 完成样本 #t 120\n");
        f.set_status_at(1, TaskStatus::Paused, 999).unwrap();
        assert_eq!(f.lines[1], "- [ ] (P1) [工作] 完成样本 #t 120 #paused");
        let reparsed = TodoFile::parse("## 日任务\n- [ ] (P1) [工作] 完成样本 #t 120 #paused\n");
        assert_eq!(reparsed.tasks[0].timer_secs, 120);
        assert_eq!(reparsed.tasks[0].status, TaskStatus::Paused);
    }

    // ── 冲突自动合并（v0.8.6）行级三方合并 T-P ─────────────────────

    /// ours==theirs：字节不变直接返回（含标签/注记的复杂行零手术）。
    #[test]
    fn merge_identical_sides_returns_ours_verbatim() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [ ] 任务A #doing #ts 1789000000 补记9/2",
            "- [ ] 任务A #doing #ts 1789000000 补记9/2",
        )
        .unwrap();
        assert_eq!(m.line, "- [ ] 任务A #doing #ts 1789000000 补记9/2");
        assert!(m.notes.is_empty());
    }

    /// base==theirs（对端未动）：保留 ours 原文，含 ours 新增注记。
    #[test]
    fn merge_base_equals_theirs_keeps_ours() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [ ] 任务A #paused 本机补记",
            "- [ ] 任务A",
        )
        .unwrap();
        assert_eq!(m.line, "- [ ] 任务A #paused 本机补记");
        assert!(m.notes.is_empty());
    }

    /// base==ours（本机未动）：整体取 theirs。
    #[test]
    fn merge_base_equals_ours_takes_theirs() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [ ] 任务A",
            "- [x] 任务A #t 300",
        )
        .unwrap();
        assert_eq!(m.line, "- [x] 任务A #t 300");
    }

    /// 状态双改不同方向（本机完成/对端进行中）→ 更高进度 Done 胜出，
    /// theirs 的 #doing/#ts 清理干净。
    #[test]
    fn merge_status_double_change_takes_higher_progress() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [x] 任务A",
            "- [ ] 任务A #doing #ts 1789000000",
        )
        .unwrap();
        assert_eq!(m.line, "- [x] 任务A");
        assert!(m.notes.is_empty());
    }

    /// 计时取 max：双侧都改 #t（200 vs 300）→ 300，状态标签组原位重写。
    #[test]
    fn merge_timer_takes_max() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A #t 100 #paused"),
            "- [ ] 任务A #t 200 #paused",
            "- [ ] 任务A #t 300 #paused",
        )
        .unwrap();
        assert_eq!(m.line, "- [ ] 任务A #t 300 #paused");
    }

    /// theirs 单侧新增标签（#blocked 原因）→ 加侧胜，原样采纳。
    #[test]
    fn merge_theirs_side_tag_adopted() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [x] 任务A",
            "- [ ] 任务A #blocked 等审批",
        )
        .unwrap();
        assert_eq!(m.line, "- [x] 任务A #blocked 等审批");
        assert!(m.notes.is_empty());
    }

    /// 内容主体双改不同 → None（真冲突，交人工）。
    #[test]
    fn merge_content_divergence_is_conflict() {
        assert!(three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [ ] 任务A 本机改写",
            "- [ ] 任务A 对端改写",
        )
        .is_none());
    }

    /// add/add（无 base）：同键新建行按属性规则合并——状态取更高进度、
    /// 计时取 max、勾选或运算。
    #[test]
    fn merge_add_add_without_base() {
        let m = three_way_merge_task_line(
            None,
            "- [ ] 任务A #t 100",
            "- [x] 任务A #t 50",
        )
        .unwrap();
        assert_eq!(m.line, "- [x] 任务A #t 100");
    }

    /// 优先级单侧改动（对端加 P0）→ 采纳对端；位置对齐 set_priority 手法。
    /// （仅 #t 的 ours 行按 Paused 解读，合并行保 Paused 态。）
    #[test]
    fn merge_priority_single_side_adopted() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [ ] 任务A #t 10",
            "- [ ] (P0) 任务A",
        )
        .unwrap();
        assert_eq!(m.line, "- [ ] (P0) 任务A #t 10 #paused");
    }

    /// 分类单侧改动（对端加 [工作]）→ 采纳对端。
    #[test]
    fn merge_category_single_side_adopted() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [ ] 任务A #t 5",
            "- [ ] [工作] 任务A",
        )
        .unwrap();
        assert_eq!(m.line, "- [ ] [工作] 任务A #t 5 #paused");
    }

    /// 优先级双改不同 → 取本机 + note 留档。
    #[test]
    fn merge_priority_double_change_keeps_ours_with_note() {
        let m = three_way_merge_task_line(
            Some("- [ ] (P2) 任务A"),
            "- [ ] (P0) 任务A",
            "- [ ] (P1) 任务A",
        )
        .unwrap();
        assert_eq!(m.line, "- [ ] (P0) 任务A");
        assert!(m.notes.iter().any(|n| n.contains("优先级")));
    }

    /// 注记保真：状态手术不得把标签后注记挤到新标签之前（否则注记被
    /// content 吞掉、身份键漂移）。
    #[test]
    fn merge_annotation_survives_tag_surgery() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A"),
            "- [ ] 任务A #doing 补记9/2",
            "- [ ] 任务A #paused",
        )
        .unwrap();
        assert_eq!(m.line, "- [ ] 任务A #doing 补记9/2");
        // 回读验证注记仍是注记、content 不变
        let f = TodoFile::parse(&format!("## 日任务\n{}\n", m.line));
        assert_eq!(f.tasks[0].display, "任务A 补记9/2");
    }

    /// #blocked 原因双改不同：取本机、对端原因落行尾注记；追加带 " #"
    /// 边界，回读不再把注记吞进受阻原因。
    #[test]
    fn merge_blocked_reason_divergence_not_absorbed_on_reparse() {
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A #blocked 旧因"),
            "- [ ] 任务A #blocked 本机因",
            "- [ ] 任务A #blocked 对端因",
        )
        .unwrap();
        assert!(m.line.contains("#blocked 本机因"));
        assert!(m.line.contains("对端受阻原因：对端因"));
        let f = TodoFile::parse(&format!("## 日任务\n{}\n", m.line));
        assert_eq!(f.tasks[0].blocked_reason.as_deref(), Some("本机因"));
    }

    /// 本机解除阻塞、对端改原因：无标签锚点时行尾附注必进 content（身份
    /// 漂移），防御校验拒绝 → 人工（解除 vs 仍在阻塞本就是意图分歧）；
    /// ours 有标签锚点时解除生效，对端文本以行尾注记无损保留。
    #[test]
    fn merge_unblocked_here_reason_changed_there() {
        // 无锚点：人工
        assert!(three_way_merge_task_line(
            Some("- [ ] 任务A #blocked 旧因"),
            "- [ ] 任务A",
            "- [ ] 任务A #blocked 新因",
        )
        .is_none());
        // 有 #t 锚点：解除生效 + 注记保留 + 回读不重新阻塞
        let m = three_way_merge_task_line(
            Some("- [ ] 任务A #blocked 旧因 #t 30"),
            "- [ ] 任务A #t 30",
            "- [ ] 任务A #blocked 新因 #t 30",
        )
        .unwrap();
        assert!(m.line.contains("对端受阻原因：新因"));
        let f = TodoFile::parse(&format!("## 日任务\n{}\n", m.line));
        assert!(f.tasks[0].blocked_reason.is_none());
        assert!(f.tasks[0].display.contains("新因"));
    }
}
