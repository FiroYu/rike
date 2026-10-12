//! 同步冲突自动合并（v0.8.6）——文件级三方合并的纯函数核心。
//!
//! 输入是 git unmerged 条目的三方字节（sync.rs 经 `run_git_raw` + `cat-file`
//! 取回，不经 trim，字节保真）；输出每文件写回字节与裁决注记。任一文件
//! 无法安全合并 → 整组 `None`（调用方 `merge --abort` 回退现状人工态）。
//!
//! # 文件策略表
//! - **TaskSemantic**：`days/*.md`、`weeks/*.md`（含 `notebooks/<id>/` 前缀）
//!   → 任务块级 diff3 三方合并
//! - **DerivedTheirsWins**：`_current.md`（外部任务每日重写的派生视图）→ theirs 胜
//! - **LineUnion**：其余 `.md/.txt` → 行级三方并集（删除不传播，双侧新增都保留）
//! - **人工**：其它扩展名 / 非 UTF-8 / BOM / 文件级 delete-vs-modify → `None`
//!
//! # 配对模型（diff3 式，base 锚点 LCS）
//! 身份键＝内容主体（`parser::task_identity`）。优先级/分类/勾选/标签都是
//! 可仲裁属性，不进键——单侧调档若拆成两个任务会直接丢掉对端勾选。
//! 同键重复按出现顺序配对，多余侧整块保留。
//!
//! # 删除与修改的裁决矩阵（任务块级）
//! | base | ours | theirs | 结果 |
//! |---|---|---|---|
//! | 有 | 删 | 未动（==base） | 删除生效 |
//! | 有 | 删 | 改（勾选等） | 保留 theirs（09-25 事故：本机 rollover 搬走、对端勾选） |
//! | 有 | 改 | 删 | 保留 ours（改动优先于删除，不丢记录） |
//! | 有 | 双改不同内容 | | 行级三方 → None → 整体人工（真冲突） |
//!
//! # add/add 文件（base 缺失，如双机离线各自建当日文件）
//! 所有块按键配对后**双侧仍有剩余** → 整体人工（双机独立起草了不同内容，
//! 保守交人工；09-29 形态＝同键全配对 → 自动合并）。
//!
//! 不做跨文件去重：同一任务完成记录两天各一份是无害冗余，跨文件比对会
//! 误杀每日重复任务。注记/受阻原因等对端用户文本并入行尾写入仓库文件
//! 本身（备份与 meta.json 会被修剪，不能作为用户文本的归宿）。
//!
//! EOL 逐行保真：未触碰行携带自己的行尾符原样输出；手术行用 ours 行尾。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::parser::{pomo_segment_key, task_identity, three_way_merge_task_line, TodoFile};

/// 一个 unmerged 路径的三方字节。`base=None` 表示 add/add；
/// `ours/theirs=None` 表示该侧删除（文件级 delete-vs-modify → 人工）。
pub struct ConflictSides {
    pub path: String,
    pub base: Option<Vec<u8>>,
    pub ours: Option<Vec<u8>>,
    pub theirs: Option<Vec<u8>>,
}

/// 一个文件的合并产物：写回工作区的最终字节 + 进 meta.json 的裁决注记。
pub struct ResolvedFile {
    pub path: String,
    pub content: Vec<u8>,
    pub notes: Vec<String>,
}

/// 整组合并（全有或全无）：任一文件人工 → `None`。
pub fn resolve_conflict_set(sides: &[ConflictSides]) -> Option<Vec<ResolvedFile>> {
    let mut out = Vec::with_capacity(sides.len());
    for s in sides {
        out.push(resolve_one(s)?);
    }
    Some(out)
}

fn resolve_one(s: &ConflictSides) -> Option<ResolvedFile> {
    let (ours_b, theirs_b) = (s.ours.as_ref()?, s.theirs.as_ref()?);
    if !is_supported_path(&s.path) {
        return None; // 扩展名/路径形态不支持 → 人工
    }
    let base_b: &[u8] = s.base.as_deref().unwrap_or(&[]);
    for raw in [base_b, ours_b, theirs_b] {
        if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return None; // BOM 文件保守交人工
        }
        if std::str::from_utf8(raw).is_err() {
            return None; // 非 UTF-8 → 人工
        }
    }
    let base = std::str::from_utf8(base_b).ok()?;
    let ours = std::str::from_utf8(ours_b).ok()?;
    let theirs = std::str::from_utf8(theirs_b).ok()?;

    if is_current_md(&s.path) {
        // 派生视图：外部任务每日整体重写，直接取对端字节
        return Some(ResolvedFile {
            path: s.path.clone(),
            content: theirs_b.clone(),
            notes: vec![],
        });
    }
    let (content, notes) = if is_task_semantic(&s.path) {
        let mut notes = Vec::new();
        let mut lines = task_semantic_merge(
            &split_lines(base),
            &split_lines(ours),
            &split_lines(theirs),
            &mut notes,
        )?;
        relocate_stranded_tasks(&mut lines, is_week_path(&s.path));
        ensure_inner_newlines(&mut lines);
        (lines.concat().into_bytes(), notes)
    } else {
        let mut notes = Vec::new();
        let lines = line_union_merge(
            &split_lines(base),
            &split_lines(ours),
            &split_lines(theirs),
            &mut notes,
        );
        (lines.concat().into_bytes(), notes)
    };
    Some(ResolvedFile { path: s.path.clone(), content, notes })
}

/// 仅接受仓库内常规文本路径：.md/.txt、无 `..`、非绝对路径。
fn is_supported_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    if !(path.ends_with(".md") || path.ends_with(".txt")) {
        return false;
    }
    !path.starts_with('/') && !path.split('/').any(|seg| seg == ".." || seg.is_empty())
}

fn is_current_md(path: &str) -> bool {
    path.replace('\\', "/").ends_with("/_current.md")
        || path.replace('\\', "/") == "_current.md"
}

/// `days/*.md` / `weeks/*.md`（含 notebooks/<id>/ 前缀）。
fn is_task_semantic(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    parts.len() >= 2
        && parts[parts.len() - 1].ends_with(".md")
        && matches!(parts[parts.len() - 2], "days" | "weeks")
}

/// 周文件判定（weeks/ 段）：决定错挂任务回插的目标段落名。
fn is_week_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.split('/').any(|seg| seg == "weeks")
}

/// add/add 错挂修复（2026-10-09 实况）：双端离线各自新建同一日/周文件时，
/// 任务块按 ours 序全部输出后、theirs 独有块追加在产物末尾——落在
/// 「## 备注」/「## 下周计划 / 备注」等非任务段下。解析层把它们当任务
/// 显示，但跨日流转不认这些段，任务因此永远滞留旧日期。这里把错挂段下
/// 的任务块挪回：未勾选 → 日任务/本周任务末尾，已勾选 → 完成事项/已
/// 完成末尾。认不出任务段或无错挂任务时原样返回——常见路径零改动。
fn relocate_stranded_tasks(out: &mut Vec<String>, week: bool) {
    let (task_sec, done_sec, homes): (&str, &str, &[&str]) = if week {
        ("本周任务", "已完成", &["本周任务", "进行中", "已完成"])
    } else {
        ("日任务", "完成事项", &["日任务", "完成事项"])
    };
    let mut file = TodoFile::parse(&out.concat());
    if file.section_flat_insert_at(task_sec).is_err() {
        return; // 模板认不出任务段 → 不动，保住既有合并产物
    }
    let stranded: Vec<usize> = file.tasks.iter()
        .filter(|t| !homes.contains(&t.section.as_str()))
        .map(|t| t.line_idx)
        .collect();
    if stranded.is_empty() {
        return;
    }
    // 降序删除避免行号漂移（delete_task 后内部重建），按原序回插
    let mut blocks: Vec<(bool, Vec<String>)> = Vec::new();
    for line_idx in stranded.into_iter().rev() {
        let checked = file.tasks.iter()
            .find(|t| t.line_idx == line_idx)
            .is_some_and(|t| t.checked);
        if let Ok(b) = file.delete_task(line_idx) {
            blocks.push((checked, b));
        }
    }
    for (checked, block) in blocks.into_iter().rev() {
        let sec = if checked && file.section_flat_insert_at(done_sec).is_ok() {
            done_sec
        } else {
            task_sec
        };
        if let Ok(at) = file.section_flat_insert_at(sec) {
            file.insert_lines_at(at, block);
        }
    }
    let lines = split_lines(&file.serialize());
    *out = lines.into_iter().map(|l| format!("{}{}", l.text, l.eol)).collect();
}

// ── 行与块 ────────────────────────────────────────────────────────

/// 一行＝正文 + 它自己的行尾符（"\n"、"\r\n" 或文件末尾的 ""）。
#[derive(PartialEq)]
struct Line {
    text: String,
    eol: String,
}

impl Line {
    fn raw(&self) -> String {
        format!("{}{}", self.text, self.eol)
    }
}

fn split_lines(s: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for part in s.split_inclusive('\n') {
        let (text, eol) = match part.strip_suffix('\n') {
            Some(t) => match t.strip_suffix('\r') {
                Some(t2) => (t2, "\r\n"),
                None => (t, "\n"),
            },
            None => (part, ""),
        };
        out.push(Line { text: text.to_string(), eol: eol.to_string() });
    }
    out
}

/// 任务块（主行+连续缩进行）或普通行；key 用于 LCS 配对。
enum Blk {
    Task { key: String, main: usize, subs: (usize, usize) },
    Plain { key: String, idx: usize },
}

impl Blk {
    fn key(&self) -> &str {
        match self {
            Blk::Task { key, .. } => key,
            Blk::Plain { key, .. } => key,
        }
    }
}

fn is_sub_line(text: &str) -> bool {
    let t = text.trim_start();
    t != text && t.starts_with("- ")
}

fn split_blocks(lines: &[Line]) -> Vec<Blk> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let text = &lines[i].text;
        if text.starts_with("- [") {
            if let Some(identity) = task_identity(text) {
                let mut j = i + 1;
                while j < lines.len() && is_sub_line(&lines[j].text) {
                    j += 1;
                }
                blocks.push(Blk::Task {
                    key: format!("T\x1f{identity}"),
                    main: i,
                    subs: (i + 1, j),
                });
                i = j;
                continue;
            }
        }
        blocks.push(Blk::Plain { key: format!("P\x1f{text}"), idx: i });
        i += 1;
    }
    blocks
}

/// LCS 配对：a 侧每个下标 → b 侧匹配下标（文本相等、保序）。O(n·m) DP，
/// 日/周文件规模（百行级）足够。
fn lcs_matches(a: &[Blk], b: &[Blk]) -> Vec<Option<usize>> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i].key() == b[j].key() {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut res = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i].key() == b[j].key() {
            res[i] = Some(j);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    res
}

// ── TaskSemantic 合并 ─────────────────────────────────────────────

/// 正文改名会脱离内容身份键。仅同一 LCS 间隙各剩一个任务，且合法番茄段
/// 未变时补配对，让另一端的结算进入行级合并；多候选与 legacy 不推断。
fn match_pomo_renames(
    base_blocks: &[Blk], side_blocks: &[Blk], base: &[Line], side: &[Line],
    matches: &mut [Option<usize>],
) {
    let anchors: Vec<(usize, usize)> = matches.iter().enumerate()
        .filter_map(|(i, &j)| j.map(|j| (i, j)))
        .chain(std::iter::once((base_blocks.len(), side_blocks.len())))
        .collect();
    let (mut bi, mut si) = (0, 0);
    for (be, se) in anchors {
        if be - bi == 1 && se - si == 1 {
            if let (Blk::Task { main: bm, .. }, Blk::Task { main: sm, .. }) =
                (&base_blocks[bi], &side_blocks[si]) {
                let key = pomo_segment_key(&base[*bm].text);
                if key.is_some() && key == pomo_segment_key(&side[*sm].text) {
                    matches[bi] = Some(si);
                }
            }
        }
        bi = be + 1;
        si = se + 1;
    }
}

fn task_semantic_merge(
    base: &[Line],
    ours: &[Line],
    theirs: &[Line],
    notes: &mut Vec<String>,
) -> Option<Vec<String>> {
    let bb = split_blocks(base);
    let ob = split_blocks(ours);
    let tb = split_blocks(theirs);
    let mut a = lcs_matches(&bb, &ob); // base → ours
    let mut b = lcs_matches(&bb, &tb); // base → theirs
    match_pomo_renames(&bb, &ob, base, ours, &mut a);
    match_pomo_renames(&bb, &tb, base, theirs, &mut b);

    let add_add_file = bb.is_empty();
    // add/add 文件里配对后双侧仍有剩余 → 人工（见模块注释）
    let mut unpaired_ours = 0usize;
    let mut unpaired_theirs = 0usize;

    let mut out: Vec<String> = Vec::new();
    let mut oi = 0usize; // ours 区块游标
    let mut ti = 0usize; // theirs 区块游标
    let mut bstart = 0usize; // 未处理的 base 区块起点

    for bi in 0..bb.len() {
        let (Some(o_m), Some(t_m)) = (a[bi], b[bi]) else { continue }; // 仅锚点
        emit_gap(
            &bb, &a, &b, bstart, bi, &ob, oi, o_m, &tb, ti, t_m, base, ours, theirs,
            &mut out, notes, add_add_file, &mut unpaired_ours, &mut unpaired_theirs,
        )?;
        // 锚点本身：三方同键，行内属性三方合并
        merge_triple(Some(&bb[bi]), &ob[o_m], &tb[t_m], base, ours, theirs, &mut out, notes)?;
        oi = o_m + 1;
        ti = t_m + 1;
        bstart = bi + 1;
    }
    emit_gap(
        &bb, &a, &b, bstart, bb.len(), &ob, oi, ob.len(), &tb, ti, tb.len(), base,
        ours, theirs, &mut out, notes, add_add_file, &mut unpaired_ours, &mut unpaired_theirs,
    )?;

    if add_add_file && unpaired_ours > 0 && unpaired_theirs > 0 {
        return None; // 双机独立起草了不同内容 → 整体人工
    }
    ensure_inner_newlines(&mut out);
    Some(out)
}

/// 落盘前换行补齐：某侧 EOF 无换行的行被排进产物中间时，直接拼接会与后续
/// 行黏成一行（如 `- [ ] A- [ ] B` 变成一条任务），非末行统一补 `\n`；
/// 末行保持原样（不虚构 EOF 换行，字节级保持原形态）。
fn ensure_inner_newlines(out: &mut [String]) {
    if let Some((_, head)) = out.split_last_mut() {
        for s in head {
            if !s.ends_with('\n') {
                s.push('\n');
            }
        }
    }
}

/// 两个锚点之间的间隙：base 侧 [bstart,bend)（无锚点块），ours/theirs 的
/// 对应区段。分类处理删除传播、双改配对、双侧新增。
#[allow(clippy::too_many_arguments)]
fn emit_gap(
    bb: &[Blk],
    a: &[Option<usize>],
    b: &[Option<usize>],
    bstart: usize,
    bend: usize,
    ob: &[Blk],
    ostart: usize,
    oend: usize,
    tb: &[Blk],
    tstart: usize,
    tend: usize,
    base: &[Line],
    ours: &[Line],
    theirs: &[Line],
    out: &mut Vec<String>,
    notes: &mut Vec<String>,
    add_add_file: bool,
    unpaired_ours: &mut usize,
    unpaired_theirs: &mut usize,
) -> Option<()> {
    // 区段内被 base 匹配占用的侧块（O_KEPT/T_KEPT 的匹配位）
    let mut o_taken = vec![false; oend - ostart];
    let mut t_taken = vec![false; tend - tstart];
    for bj in bstart..bend {
        if let Some(om) = a[bj] {
            if om >= ostart && om < oend {
                o_taken[om - ostart] = true;
            }
        }
        if let Some(tm) = b[bj] {
            if tm >= tstart && tm < tend {
                t_taken[tm - tstart] = true;
            }
        }
    }

    // 1) base 块按原序：删除传播 / 保留改动侧
    for bj in bstart..bend {
        match (a[bj], b[bj]) {
            (Some(om), None) => {
                // theirs 删：ours 未动（==base）→ 删除生效；ours 有改动 → 保留 ours
                if !block_equal(&bb[bj], &ob[om], base, ours) {
                    push_block(&ob[om], ours, out);
                    note_block(bb, bj, notes, "对端已删除但本机有改动，保留本机版本");
                }
            }
            (None, Some(tm)) => {
                // ours 删（搬走/移除）：theirs 未动 → 删除生效；有改动 → 保留 theirs
                if !block_equal(&bb[bj], &tb[tm], base, theirs) {
                    push_block(&tb[tm], theirs, out);
                    note_block(bb, bj, notes, "本机已删除但对端有改动，保留对端版本");
                }
            }
            (Some(_), Some(_)) => unreachable!("双侧匹配即锚点，不进间隙"),
            (None, None) => {} // 双改块，第 2) 步处理
        }
    }

    // 2) 双改块（base 有、双侧键都变了）按序与双侧区段余量配对：
    //    第 i 个双改块 ↔ 第 i 个未占用侧块；行级三方，内容分歧 → None
    let d_idx: Vec<usize> = (bstart..bend)
        .filter(|&bj| a[bj].is_none() && b[bj].is_none())
        .collect();
    let mut o_free = (0..oend - ostart).filter(|&i| !o_taken[i]).collect::<Vec<_>>();
    let mut t_free = (0..tend - tstart).filter(|&i| !t_taken[i]).collect::<Vec<_>>();
    let pairs = d_idx.len().min(o_free.len()).min(t_free.len());
    for k in 0..pairs {
        let (oi_, ti_) = (o_free[k], t_free[k]);
        merge_triple(
            Some(&bb[d_idx[k]]), &ob[ostart + oi_], &tb[tstart + ti_],
            base, ours, theirs, out, notes,
        )?;
    }
    o_free.drain(0..pairs);
    t_free.drain(0..pairs);

    // 3) 剩余双侧新增：先按键配对（add/add 同键 → 三方合并，base=None），
    //    各自配不上的是纯新增
    let mut t_used = vec![false; t_free.len()];
    for &oi_ in &o_free {
        let oblk = &ob[ostart + oi_];
        let mut mate = None;
        for (n, &ti_) in t_free.iter().enumerate() {
            if !t_used[n] && tb[tstart + ti_].key() == oblk.key() {
                mate = Some((n, ti_));
                break;
            }
        }
        match mate {
            Some((n, ti_)) => {
                t_used[n] = true;
                merge_triple(None, oblk, &tb[tstart + ti_], base, ours, theirs, out, notes)?;
            }
            None => {
                push_block(oblk, ours, out);
                *unpaired_ours += 1;
            }
        }
    }
    for (n, &ti_) in t_free.iter().enumerate() {
        if !t_used[n] {
            push_block(&tb[tstart + ti_], theirs, out);
            *unpaired_theirs += 1;
        }
    }
    let _ = add_add_file; // 计数在调用方裁决
    Some(())
}

/// 锚点（三方同键）：行内属性三方合并 + 子行并集（调用方直接走 merge_triple）。
/// 一个三方块组的合并产出（base 可为 None=add/add）。
#[allow(clippy::too_many_arguments)]
fn merge_triple(
    b: Option<&Blk>,
    o: &Blk,
    t: &Blk,
    base: &[Line],
    ours: &[Line],
    theirs: &[Line],
    out: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> Option<()> {
    match (o, t) {
        (Blk::Plain { idx: oi_, .. }, Blk::Plain { idx: ti_, .. }) => {
            let (ot, tt) = (ours[*oi_].text.as_str(), theirs[*ti_].text.as_str());
            if ot == tt {
                out.push(ours[*oi_].raw());
            } else {
                // 非任务行双改不同：ours 保位 + theirs 紧随 + note
                out.push(ours[*oi_].raw());
                out.push(theirs[*ti_].raw());
                notes.push(format!("非任务行双端改动不同，两行均保留（本机「{ot}」/ 对端「{tt}」）"));
            }
            Some(())
        }
        (Blk::Task { main: om, subs: os, .. }, Blk::Task { main: tm, subs: ts, .. }) => {
            let base_line = b.and_then(|blk| match blk {
                Blk::Task { main: bm, .. } => Some(base[*bm].text.as_str()),
                Blk::Plain { .. } => None,
            });
            let merged =
                three_way_merge_task_line(base_line, &ours[*om].text, &theirs[*tm].text)?;
            for n in &merged.notes {
                notes.push(format!("{}：{n}", ours[*om].text.trim()));
            }
            out.push(format!("{}{}", merged.line, ours[*om].eol));
            let b_subs = b.and_then(|blk| match blk {
                Blk::Task { subs: bs, .. } => Some(raw_slice(base, *bs)),
                Blk::Plain { .. } => None,
            });
            let o_subs = raw_slice(ours, *os);
            let t_subs = raw_slice(theirs, *ts);
            out.extend(merge_sub_lines(b_subs.as_deref(), &o_subs, &t_subs));
            Some(())
        }
        // 任务键只在同侧结构里对齐；跨类型组不发生（键命名空间隔离）
        _ => {
            push_block(o, ours, out);
            push_block(t, theirs, out);
            notes.push("任务块与普通行对置，双侧均保留".into());
            Some(())
        }
    }
}

fn raw_slice(lines: &[Line], range: (usize, usize)) -> Vec<String> {
    lines[range.0..range.1].iter().map(Line::raw).collect()
}

/// 子行三方：一侧改动（含删除/清空）而另一侧未动 → 改动侧生效（删除传播，
/// 与任务块级规则一致，否则「本机删备注、对端调档」会把旧备注复活推走）；
/// 两侧都动 → 并集（ours 序 + theirs 独有追加，宁重复不丢）。
fn merge_sub_lines(base: Option<&[String]>, ours: &[String], theirs: &[String]) -> Vec<String> {
    if ours == theirs {
        return ours.to_vec();
    }
    if let Some(base) = base {
        if ours == base {
            return theirs.to_vec(); // 仅对端动（含删/增/改子行）→ 对端生效
        }
        if theirs == base {
            return ours.to_vec(); // 仅本机动（含删除）→ 本机生效
        }
    }
    let strip = |s: &str| s.trim_end_matches(['\r', '\n']).to_string();
    let mut out = ours.to_vec();
    for t in theirs {
        let tt = strip(t);
        if !out.iter().any(|o| strip(o) == tt) {
            out.push(t.clone());
        }
    }
    out
}

/// 块整体原文（主行+子行）。
fn push_block(blk: &Blk, lines: &[Line], out: &mut Vec<String>) {
    match blk {
        Blk::Task { main, subs, .. } => {
            out.push(lines[*main].raw());
            out.extend(raw_slice(lines, *subs));
        }
        Blk::Plain { idx, .. } => out.push(lines[*idx].raw()),
    }
}

fn block_equal(x: &Blk, y: &Blk, xl: &[Line], yl: &[Line]) -> bool {
    match (x, y) {
        (Blk::Task { main: xm, subs: xs, .. }, Blk::Task { main: ym, subs: ys, .. }) => {
            xl[*xm].text == yl[*ym].text && xl[xs.0..xs.1] == yl[ys.0..ys.1]
        }
        (Blk::Plain { idx: xi, .. }, Blk::Plain { idx: yi, .. }) => xl[*xi].text == yl[*yi].text,
        _ => false,
    }
}

fn note_block(bb: &[Blk], bj: usize, notes: &mut Vec<String>, msg: &str) {
    let subject = match &bb[bj] {
        Blk::Task { key, .. } => key.trim_start_matches("T\x1f").to_string(),
        Blk::Plain { .. } => "非任务行".to_string(),
    };
    notes.push(format!("「{subject}」{msg}"));
}

// ── LineUnion 合并（其余 .md/.txt）───────────────────────────────

/// 行级三方并集：锚点保位，间隙 ours 区段先出、theirs 独有行紧随。
/// 删除不传播（备注类宁重复不丢失）。
fn line_union_merge(
    base: &[Line],
    ours: &[Line],
    theirs: &[Line],
    notes: &mut Vec<String>,
) -> Vec<String> {
    // 行文本 LCS（复用块 LCS 的 DP，构造临时键）
    let to_plain_blocks = |ls: &[Line]| {
        ls.iter()
            .enumerate()
            .map(|(i, l)| Blk::Plain { key: format!("P\x1f{}", l.text), idx: i })
            .collect::<Vec<_>>()
    };
    let (bb, ob, tb) = (to_plain_blocks(base), to_plain_blocks(ours), to_plain_blocks(theirs));
    let a = lcs_matches(&bb, &ob);
    let b = lcs_matches(&bb, &tb);

    let mut out: Vec<String> = Vec::new();
    let mut oi = 0usize;
    let mut ti = 0usize;
    let mut both_new = 0usize;
    for bi in 0..bb.len() {
        let (Some(o_m), Some(t_m)) = (a[bi], b[bi]) else { continue };
        // ours 区段先出，theirs 区段里 ours 已有文本的去重
        let o_texts: Vec<&str> = ours[oi..o_m].iter().map(|l| l.text.as_str()).collect();
        for l in &ours[oi..o_m] {
            out.push(l.raw());
        }
        for l in &theirs[ti..t_m] {
            if !o_texts.contains(&l.text.as_str()) {
                out.push(l.raw());
                both_new += 1;
            }
        }
        out.push(ours[o_m].raw());
        oi = o_m + 1;
        ti = t_m + 1;
    }
    let o_texts: Vec<&str> = ours[oi..].iter().map(|l| l.text.as_str()).collect();
    for l in &ours[oi..] {
        out.push(l.raw());
    }
    for l in &theirs[ti..] {
        if !o_texts.contains(&l.text.as_str()) {
            out.push(l.raw());
            both_new += 1;
        }
    }
    if both_new > 0 {
        notes.push(format!("并集合并：对端独有 {both_new} 行已追加保留"));
    }
    ensure_inner_newlines(&mut out);
    out
}

// ── 备份 ──────────────────────────────────────────────────────────

pub const BACKUP_KEEP: usize = 10;

/// 备份一组冲突四方字节到 `<root>\conflict-backups\<ts>-<n>files\<相对路径>.{base,ours,theirs,resolved}`
/// + `meta.json`（含裁决注记），按目录名保留最近 [`BACKUP_KEEP`] 组。
///
/// 失败返回 Err（调用方记日志，不阻断合并）。
pub fn write_backups(
    root: Option<&Path>,
    sides: &[ConflictSides],
    resolved: &[ResolvedFile],
) -> Result<(), String> {
    let Some(root) = root else { return Ok(()) }; // 无 APPDATA 环境（Android）→ 跳过
    let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let ts = dur.as_secs();
    // 秒级 + 毫秒：同一秒内两组合并不会写进同一目录互相覆盖
    let group = root
        .join("conflict-backups")
        .join(format!("{}.{:03}-{}files", ts, dur.subsec_millis(), sides.len()));

    for (s, r) in sides.iter().zip(resolved.iter()) {
        let rel = s.path.replace('\\', "/");
        let target = group.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("备份建目录失败 {parent:?}: {e}"))?;
        }
        // 保留相对目录结构，后缀直接接在文件名后：group/<rel>.<suffix>
        let write = |suffix: &str, data: &[u8]| -> Result<(), String> {
            let path = std::path::PathBuf::from(format!("{}.{}", target.display(), suffix));
            std::fs::write(&path, data).map_err(|e| format!("备份写入失败 {}.{suffix}: {e}", s.path))
        };
        if let Some(base) = &s.base {
            write("base", base)?;
        }
        write("ours", s.ours.as_deref().unwrap_or(&[]))?;
        write("theirs", s.theirs.as_deref().unwrap_or(&[]))?;
        write("resolved", &r.content)?;
    }

    let meta = serde_json::json!({
        "timestamp": ts,
        "files": resolved
            .iter()
            .map(|r| serde_json::json!({ "path": r.path, "notes": r.notes }))
            .collect::<Vec<_>>(),
    });
    std::fs::write(
        group.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("备份 meta.json 写入失败: {e}"))?;

    prune_backups(&root.join("conflict-backups"));
    Ok(())
}

/// 按目录名（Unix 秒前缀，秒.毫秒-份数files）保留最近 keep 组，超出删除（尽力而为）。
fn prune_backups(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut groups: Vec<(u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let secs = name.split(['.', '-']).next()?.parse::<u64>().ok()?;
            name.ends_with("files").then_some((secs, e.path()))
        })
        .collect();
    groups.sort_by_key(|(secs, _)| std::cmp::Reverse(*secs));
    for (_, path) in groups.iter().skip(BACKUP_KEEP) {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pomo_text_edit_and_settlement_merge_as_one_task() {
        let base = "## 日任务\n- [ ] (P1) A #pomo 0 #doing #ts 100\n";
        let renamed = "## 日任务\n- [ ] (P0) B #pomo 0 #doing #ts 100\n";
        let settled = "## 日任务\n- [ ] (P1) A #pomo 1\n";
        for (o, t) in [(renamed, settled), (settled, renamed)] {
            let merged = resolved_content(&sides("days/2026-10-09.md", Some(base), o, t)).unwrap();
            assert_eq!(merged, "## 日任务\n- [ ] (P0) B #pomo 1\n");
        }
    }

    #[test]
    fn pomo_priority_edit_and_next_segment_keep_new_start() {
        let base = "- [ ] (P1) A #pomo 0 #doing #ts 100\n";
        let ours = "- [ ] (P0) A #pomo 0 #doing #ts 100\n";
        let theirs = "- [ ] (P1) A #pomo 1 #doing #ts 1600\n";
        for (o, t) in [(ours, theirs), (theirs, ours)] {
            let merged = resolved_content(&sides("days/2026-10-09.md", Some(base), o, t)).unwrap();
            assert_eq!(merged, "- [ ] (P0) A #pomo 1 #doing #ts 1600\n");
        }
    }

    #[test]
    fn pomo_rename_does_not_guess_multiple_candidates_or_different_segments() {
        let base = "- [ ] A #pomo 0 #doing #ts 100\n";
        let settled = "- [ ] A #pomo 1\n";
        for ours in [
            "- [ ] B #pomo 0 #doing #ts 101\n",
            "- [ ] B #pomo 0 #doing #ts 100\n- [ ] C #pomo 0 #doing #ts 100\n",
        ] {
            let merged = resolved_content(&sides("days/2026-10-09.md", Some(base), ours, settled)).unwrap();
            assert!(merged.contains("- [ ] A #pomo 1\n"));
            assert!(merged.contains("- [ ] B #pomo 0 #doing"));
        }
        let ours = "- [ ] B #pomo 0 #doing #ts 100\n";
        let theirs = "- [ ] C #pomo 0 #doing #ts 100\n";
        assert!(resolved_content(&sides("days/2026-10-09.md", Some(base), ours, theirs)).is_none());
    }

    fn sides(path: &str, base: Option<&str>, ours: &str, theirs: &str) -> ConflictSides {
        ConflictSides {
            path: path.to_string(),
            base: base.map(|s| s.as_bytes().to_vec()),
            ours: Some(ours.as_bytes().to_vec()),
            theirs: Some(theirs.as_bytes().to_vec()),
        }
    }

    fn resolved_content(s: &ConflictSides) -> Option<String> {
        resolve_conflict_set(std::slice::from_ref(s))
            .map(|v| String::from_utf8(v[0].content.clone()).unwrap())
    }

    const D0925_BASE: &str = "# 2026-09-25\n\n## 日任务\n- [ ] (P1) 周报\n";
    const D0925_OURS: &str = "# 2026-09-25\n\n## 日任务\n";
    const D0925_THEIRS: &str = "# 2026-09-25\n\n## 日任务\n- [x] (P1) 周报\n";

    /// 09-25 事故：本机 rollover 搬走（文件内删除）、家用机勾选 → 对端勾选块保留。
    #[test]
    fn accident_0925_theirs_checked_survives_ours_move() {
        let got = resolved_content(&sides("days/2026-09-25.md", Some(D0925_BASE), D0925_OURS, D0925_THEIRS));
        assert_eq!(got.unwrap(), D0925_THEIRS);
    }

    /// 删除传播：对端未动（==base）时本机删除生效，不复活。
    #[test]
    fn deletion_propagates_when_other_side_unchanged() {
        let got = resolved_content(&sides("days/2026-09-25.md", Some(D0925_BASE), D0925_OURS, D0925_BASE));
        assert_eq!(got.unwrap(), D0925_OURS);
    }

    /// 09-29 事故：base 无此文件，双机各自录入同 6 任务（其一勾选）→ 全保留且勾选生效。
    #[test]
    fn accident_0929_add_add_same_keys_all_kept() {
        let ours = "# 2026-09-29\n\n## 日任务\n- [ ] 任务1\n- [ ] 任务2\n- [ ] 任务3\n- [ ] 任务4\n- [ ] 任务5\n- [ ] 任务6\n";
        let theirs = "# 2026-09-29\n\n## 日任务\n- [ ] 任务1\n- [x] 任务2\n- [ ] 任务3\n- [ ] 任务4\n- [ ] 任务5\n- [ ] 任务6\n";
        let got = resolved_content(&sides("days/2026-09-29.md", None, ours, theirs)).unwrap();
        for i in 1..=6 {
            assert!(got.contains(&format!("任务{i}")), "缺任务{i}");
        }
        assert!(got.contains("- [x] 任务2"));
        assert_eq!(got.matches("- [x]").count(), 1);
    }

    /// add/add 双机独立起草不同内容 → 整体人工（real_conflict 契约）。
    #[test]
    fn add_add_divergent_content_is_manual() {
        let ours = "# 冲突\n\n## 日任务\n- [ ] (P1) A 的版本\n";
        let theirs = "# 冲突\n\n## 日任务\n- [ ] (P1) B 的版本\n";
        let got = resolved_content(&sides("days/2026-09-11.md", None, ours, theirs));
        assert!(got.is_none());
    }

    /// 有 base 的同任务双改：本机改措辞 + 对端勾选 → 两行都保留（X5 契约）。
    #[test]
    fn modify_vs_check_keeps_both_lines() {
        let base = "# D\n\n## 日任务\n- [ ] 旧措辞\n";
        let ours = "# D\n\n## 日任务\n- [ ] 新措辞\n";
        let theirs = "# D\n\n## 日任务\n- [x] 旧措辞\n";
        let got = resolved_content(&sides("days/2026-09-11.md", Some(base), ours, theirs)).unwrap();
        assert!(got.contains("- [x] 旧措辞"));
        assert!(got.contains("- [ ] 新措辞"));
    }

    /// 内容双改不同（有 base）→ 人工。
    #[test]
    fn content_double_edit_with_base_is_manual() {
        let base = "# 冲突\n\n## 日任务\n- [ ] (P1) 原始\n";
        let ours = "# 冲突\n\n## 日任务\n- [ ] (P1) A 的版本\n";
        let theirs = "# 冲突\n\n## 日任务\n- [ ] (P1) B 的版本\n";
        assert!(resolved_content(&sides("days/2026-09-11.md", Some(base), ours, theirs)).is_none());
    }

    /// theirs 独有任务落在其所属段落内（下一节标题之前）。
    #[test]
    fn theirs_only_task_lands_in_its_section() {
        let base = "# D\n\n## 日任务\n- [ ] 任务A\n\n## 备注\n保留\n";
        let theirs = "# D\n\n## 日任务\n- [ ] 任务A\n- [ ] 任务B\n\n## 备注\n保留\n";
        let got = resolved_content(&sides("days/2026-09-11.md", Some(base), base, theirs)).unwrap();
        let a = got.find("任务A").unwrap();
        let b = got.find("任务B").unwrap();
        let m = got.find("## 备注").unwrap();
        assert!(a < b && b < m, "任务B 应在日任务段内：\n{got}");
    }

    /// 2026-10-09 实况回归：双机离线各自新建同一日文件（本机 3 条、笔电 7 条），
    /// ours 块全部配对、theirs 多出的任务块被追加到产物末尾——落在「## 备注」
    /// 之后。解析层把它们当任务显示，但跨日流转只认任务段，任务永远滞留
    /// 旧日期（用户次日发现昨天工作没搬运）。
    #[test]
    fn add_add_theirs_extra_tasks_join_day_task_section() {
        let file = |n: usize| {
            format!(
                "# 2026-10-09 (周五)\n\n> 本日重点: \n\n## 日任务\n<!-- 追加格式: -->\n{}## 完成事项\n\n## 备注\n",
                (0..n).map(|i| format!("- [ ] (P1) [工作] 任务{i}\n")).collect::<String>()
            )
        };
        let got = resolved_content(&sides(
            "notebooks/nb-x/days/2026-10-09.md",
            None,
            &file(3),
            &file(7),
        ))
        .unwrap();
        let task_sec = &got[..got.find("## 完成事项").unwrap()];
        for i in 0..7 {
            assert!(task_sec.contains(&format!("任务{i}")), "任务{i} 应在日任务段：\n{got}");
        }
        let notes = &got[got.find("## 备注").unwrap()..];
        assert!(!notes.contains("- ["), "备注段不应残留任务行：\n{notes}");
    }

    /// 周文件同病：add/add 后 theirs 独有任务（含已勾选）不得落在
    /// 「## 下周计划 / 备注」下；未勾选进本周任务，已勾选进已完成。
    #[test]
    fn add_add_week_stranded_tasks_rejoin_task_sections() {
        let ours = "# 2026-W41\n\n## 本周任务\n- [ ] 周任务A\n\n## 进行中\n\n## 已完成\n\n## 下周计划 / 备注\n";
        let theirs = "# 2026-W41\n\n## 本周任务\n- [ ] 周任务A\n- [ ] 周任务B\n- [x] 周任务C\n\n## 进行中\n\n## 已完成\n\n## 下周计划 / 备注\n";
        let got = resolved_content(&sides(
            "notebooks/nb-x/weeks/2026-W41.md",
            None,
            ours,
            theirs,
        ))
        .unwrap();
        let (b, c) = (got.find("周任务B").unwrap(), got.find("周任务C").unwrap());
        let (done, plan) = (got.find("## 已完成").unwrap(), got.find("## 下周计划").unwrap());
        assert!(b < done, "周任务B 应在本周任务段：\n{got}");
        assert!(done < c && c < plan, "周任务C（已勾选）应在已完成段：\n{got}");
    }

    /// 认不出「## 日任务」段（如自定义模板）→ 不挪动、不失败，产物保持合并原样。
    #[test]
    fn day_without_task_section_keeps_merge_output() {
        let ours = "# 笔记\n\n## 待办\n- [ ] A\n";
        let theirs = "# 笔记\n\n## 待办\n- [ ] A\n- [ ] B\n";
        let got = resolved_content(&sides("days/2026-10-09.md", Some(ours), ours, theirs));
        let got = got.unwrap();
        assert!(got.contains("- [ ] A") && got.contains("- [ ] B"), "产物应含双侧任务：\n{got}");
    }

    /// 同键任务的子行并集：ours 序 + theirs 独有追加。
    #[test]
    fn sub_lines_union_on_matched_task() {
        let base = "# D\n\n## 日任务\n- [ ] 任务A\n  - 步骤1\n";
        let theirs = "# D\n\n## 日任务\n- [ ] 任务A\n  - 步骤1\n  - 步骤2\n";
        let got = resolved_content(&sides("days/2026-09-11.md", Some(base), base, theirs)).unwrap();
        assert!(got.contains("  - 步骤1"));
        assert!(got.contains("  - 步骤2"));
        assert!(got.find("步骤1").unwrap() < got.find("步骤2").unwrap());
    }

    /// 同键重复任务按出现顺序配对，互不吞并。
    #[test]
    fn duplicate_keys_pair_positionally() {
        let base = "# D\n\n## 日任务\n- [ ] 喝水\n- [ ] 喝水\n";
        let ours = "# D\n\n## 日任务\n- [x] 喝水\n- [ ] 喝水\n";
        let theirs = "# D\n\n## 日任务\n- [ ] 喝水\n- [x] 喝水\n";
        let got = resolved_content(&sides("days/2026-09-11.md", Some(base), ours, theirs)).unwrap();
        assert_eq!(got.matches("- [x] 喝水").count(), 2, "各自勾选的副本都应完成：\n{got}");
        assert_eq!(got.matches("- [ ] 喝水").count(), 0);
    }

    /// 非任务行双改不同：两行均保留 + note。
    #[test]
    fn plain_line_double_edit_unions_with_note() {
        let base = "# D\n\n## 日任务\n本日重点：旧\n";
        let ours = "# D\n\n## 日任务\n本日重点：本机改\n";
        let theirs = "# D\n\n## 日任务\n本日重点：对端改\n";
        let s = sides("days/2026-09-11.md", Some(base), ours, theirs);
        let out = resolve_conflict_set(&[s]).unwrap();
        let text = String::from_utf8(out[0].content.clone()).unwrap();
        assert!(text.contains("本机改") && text.contains("对端改"));
        assert!(out[0].notes.iter().any(|n| n.contains("两行均保留")));
    }

    /// _current.md 派生视图：theirs 整体胜出（字节级）。
    #[test]
    fn current_md_theirs_wins() {
        let base = "# cur\n旧内容\n";
        let ours = "# cur\n本机重写\n";
        let theirs = "# cur\n外部重写\n";
        let got = resolved_content(&sides("notebooks/nb1/_current.md", Some(base), ours, theirs));
        assert_eq!(got.unwrap(), theirs);
    }

    /// LineUnion（notes 等）：双侧新增行都保留。
    #[test]
    fn line_union_keeps_both_sides() {
        let base = "# 笔记\n- 既有\n";
        let ours = "# 笔记\n- 既有\n- 本机新增\n";
        let theirs = "# 笔记\n- 既有\n- 对端新增\n";
        let got = resolved_content(&sides("notes/2026-09-11.md", Some(base), ours, theirs)).unwrap();
        assert!(got.contains("本机新增") && got.contains("对端新增"));
    }

    /// 文件级 delete-vs-modify → 人工。
    #[test]
    fn file_level_delete_vs_modify_is_manual() {
        let mut s = sides("days/2026-09-11.md", Some("x\n"), "y\n", "z\n");
        s.ours = None;
        let out = resolve_conflict_set(&[s]);
        assert!(out.is_none());
    }

    /// 不支持的扩展名 → 人工。
    #[test]
    fn unsupported_extension_is_manual() {
        assert!(resolved_content(&sides("scripts/tool.py", Some("a\n"), "b\n", "c\n")).is_none());
    }

    /// 非 UTF-8 → 人工。
    #[test]
    fn non_utf8_is_manual() {
        let mut s = sides("days/2026-09-11.md", Some("a\n"), "b\n", "c\n");
        s.theirs = Some(vec![0xFF, 0xFE, 0x00]);
        assert!(resolve_conflict_set(&[s]).is_none());
    }

    /// BOM → 人工（保守）。
    #[test]
    fn bom_is_manual() {
        let mut s = sides("days/2026-09-11.md", Some("a\n"), "b\n", "c\n");
        s.ours = Some("\u{FEFF}b\n".as_bytes().to_vec());
        assert!(resolve_conflict_set(&[s]).is_none());
    }

    /// EOL 逐行保真：未触碰行保留自身行尾；手术行用 ours 行尾。
    #[test]
    fn eol_preserved_per_line() {
        let base = "# D\n\n## 日任务\n- [ ] 任务A\n";
        let ours = "# D\r\n\r\n## 日任务\n- [ ] 任务A\n"; // 混合行尾
        let theirs = "# D\n\n## 日任务\n- [x] 任务A\r\n";
        let got = resolved_content(&sides("days/2026-09-11.md", Some(base), ours, theirs)).unwrap();
        assert!(got.starts_with("# D\r\n"), "未触碰行保留 CRLF");
        assert!(got.contains("- [x] 任务A\n"), "合并行用 ours 的 LF：\n{got:?}");
    }

    /// 全有或全无：一组里任一文件人工 → 整组 None。
    #[test]
    fn all_or_nothing_across_set() {
        let ok = sides("days/2026-09-11.md", Some(D0925_BASE), D0925_OURS, D0925_THEIRS);
        let bad = sides("scripts/x.py", Some("a\n"), "b\n", "c\n");
        assert!(resolve_conflict_set(&[ok, bad]).is_none());
    }

    /// 两天不去重：同键任务在两个文件各自独立合并。
    #[test]
    fn no_cross_file_dedup() {
        let s1 = sides("days/2026-09-25.md", Some(D0925_BASE), D0925_OURS, D0925_THEIRS);
        let base2 = "# 2026-09-26\n\n## 日任务\n- [ ] (P1) 周报\n";
        let s2 = sides(
            "days/2026-09-26.md",
            Some(base2),
            "# 2026-09-26\n\n## 日任务\n",
            "# 2026-09-26\n\n## 日任务\n- [x] (P1) 周报\n",
        );
        let out = resolve_conflict_set(&[s1, s2]).unwrap();
        assert_eq!(out.len(), 2);
        assert!(String::from_utf8(out[1].content.clone()).unwrap().contains("- [x] (P1) 周报"));
    }

    /// 备份写盘 + 修剪到 10 组。
    #[test]
    fn backups_written_and_pruned() {
        let root = std::env::temp_dir().join(format!("sticky-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for round in 0..12 {
            let s = ConflictSides {
                path: "days/x.md".into(),
                base: Some(b"b\n".to_vec()),
                ours: Some(b"o\n".to_vec()),
                theirs: Some(b"t\n".to_vec()),
            };
            let r = ResolvedFile { path: "days/x.md".into(), content: b"m\n".to_vec(), notes: vec!["注".into()] };
            write_backups(Some(&root), &[s], &[r]).unwrap();
            // 目录名含秒级时间戳，同秒内多组靠遍历计数区分——测试里人为错开
            std::thread::sleep(std::time::Duration::from_millis(1100));
            let _ = round;
        }
        let entries: Vec<_> = std::fs::read_dir(root.join("conflict-backups"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), BACKUP_KEEP, "应保留 10 组：{entries:?}");
        // 每组含 4 份文件（保留 days/ 相对结构）+ meta.json
        let group = root.join("conflict-backups").join(entries.iter().max().unwrap());
        let mut names: Vec<String> = std::fs::read_dir(group.join("days"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert!(names.contains(&"x.md.base".to_string()), "{names:?}");
        assert!(names.contains(&"x.md.ours".to_string()));
        assert!(names.contains(&"x.md.theirs".to_string()));
        assert!(names.contains(&"x.md.resolved".to_string()));
        assert!(group.join("meta.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 某侧 EOF 无换行的行排进产物中间时补 `\n`，两条任务不黏成一行。
    #[test]
    fn eof_without_newline_does_not_glue_following_add() {
        let s = sides(
            "days/2026-09-30.md",
            Some("## 日任务\n"),
            "## 日任务\n- [ ] 任务A",     // 本机末行无换行
            "## 日任务\n- [ ] 任务B\n",
        );
        let got = resolved_content(&s).unwrap();
        assert_eq!(got, "## 日任务\n- [ ] 任务A\n- [ ] 任务B\n");
    }

    /// ours 任务行 EOF 无换行、theirs 加了子行：合并行与子行间补换行。
    #[test]
    fn theirs_sub_lines_after_ours_eof_line_get_newline() {
        let s = sides(
            "days/2026-09-30.md",
            Some("## 日任务\n- [ ] 任务A\n"),
            "## 日任务\n- [ ] (P1) 任务A", // EOF 无换行
            "## 日任务\n- [ ] 任务A\n  - 子行备注\n",
        );
        let got = resolved_content(&s).unwrap();
        assert_eq!(got, "## 日任务\n- [ ] (P1) 任务A\n  - 子行备注\n");
    }

    /// 子行删除传播：一侧删子行、另一侧子行未动 → 删除生效（两个方向），
    /// 不再把旧备注复活推走。
    #[test]
    fn sub_line_deletion_propagates_when_other_side_subs_untouched() {
        let base = "## 日任务\n- [ ] 任务A\n  - 旧备注\n";
        // 本机删子行+调档，对端整体未动
        let got = resolved_content(&sides(
            "days/2026-09-11.md",
            Some(base),
            "## 日任务\n- [ ] (P1) 任务A\n",
            base,
        ))
        .unwrap();
        assert!(got.contains("(P1)"));
        assert!(!got.contains("旧备注"), "对端未动子行时删除应生效：\n{got}");
        // 对端删子行，本机只调档
        let got = resolved_content(&sides(
            "days/2026-09-11.md",
            Some(base),
            "## 日任务\n- [ ] (P1) 任务A\n  - 旧备注\n",
            "## 日任务\n- [ ] 任务A\n",
        ))
        .unwrap();
        assert!(!got.contains("旧备注"), "本机子行未动时对端删除应生效：\n{got}");
    }
}
