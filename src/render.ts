/** 纯渲染：状态 → DOM。无副作用、无 invoke；交互由 main.ts 事件委托处理。 */

import type { Category, LeftoversDto, Notebook, NotesDto, SyncState, TaskStatus, TaskView, ViewKind } from "./api";

const WEEKDAY_CN = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
export const TOUCH_DEVICE = window.matchMedia('(hover: none)').matches;

export function renderSyncSettings(panel: HTMLElement): void {
  const settings = document.createElement("div");
  settings.className = "sync-settings";
  const patHidden = !/android/i.test(navigator.userAgent) ? "hidden" : "";
  settings.innerHTML = `
    <button id="sync-settings-toggle" class="notebook-option" type="button" aria-expanded="false" aria-controls="sync-settings-form">同步设置</button>
    <form id="sync-settings-form" hidden>
      <label for="sync-repo-url">同步仓库</label>
      <input id="sync-repo-url" type="text" readonly aria-readonly="true">
      <label for="sync-pat" ${patHidden}>PAT <span id="sync-pat-set" hidden>已设置</span></label>
      <input ${patHidden} id="sync-pat" type="password" placeholder="GitHub Fine-grained PAT" autocomplete="new-password" spellcheck="false" autocapitalize="off">
      <p ${patHidden}>同步令牌仅安卓端需要</p>
      <div class="sync-settings-actions"><button type="submit">保存</button><button id="sync-test" type="button">测试连接</button></div>
      <p id="sync-settings-status" role="status" aria-live="polite"></p>
    </form>`;
  panel.append(settings);
}

const CATEGORY_LABEL: Record<Category, string> = {
  Work: "工 作",
  Personal: "个 人",
  Uncategorized: "未分类",
};

const CATEGORY_ORDER: Category[] = ["Work", "Personal", "Uncategorized"];

/** 优先级样式档（原型 B：mono 文本，P0 红）。 */
const PRIO_CLASS: Record<string, string> = { P0: "p0", P1: "p1", P2: "p2", P3: "p3" };
// 显示序键：优先级 P0→P3→无；无优先级排最后（rank=4）。
const PRIORITY_RANK: Record<string, number> = { P0: 0, P1: 1, P2: 2, P3: 3 };

/** v0.8 状态四态文案与符号（与 Rust TIMER_CAP_SECS 同值的展示上限）。 */
export const TIMER_CAP_SECS = 359999;
const STATUS_LABEL: Record<TaskStatus, string> = { todo: "待开始", doing: "进行中", paused: "暂停", done: "完成" };
const ST_SYMBOL: Record<TaskStatus, string> = { todo: "○", doing: "▶", paused: "‖", done: "○" };
const ST_TITLE: Record<TaskStatus, string> = {
  todo: "点击开始（→ 进行中）",
  doing: "点击暂停",
  paused: "点击继续",
  done: "完成（取消行尾勾选复活，回暂停）",
};

/** 计时显示值 = 已结算累计 + 进行中增量（时钟倒拨取 0），封顶 99:59:59。 */
export function timerDisplaySecs(task: TaskView, nowMs: number): number {
  const delta = task.status === "doing" && task.timer_started_at !== null
    ? Math.max(0, Math.floor(nowMs / 1000) - task.timer_started_at)
    : 0;
  return Math.min(task.timer_secs + delta, TIMER_CAP_SECS);
}

/** 固定八位 HH:MM:SS 等宽展示。 */
export function formatHms(totalSecs: number): string {
  const s = Math.min(Math.max(Math.trunc(totalSecs), 0), TIMER_CAP_SECS);
  const h = String(Math.floor(s / 3600)).padStart(2, "0");
  const m = String(Math.floor((s % 3600) / 60)).padStart(2, "0");
  const x = String(s % 60).padStart(2, "0");
  return `${h}:${m}:${x}`;
}

export interface FoldState {
  folded: Set<Category>;
  overdueOpen: boolean;
}

export interface UiState {
  notebookId: string;
  notebooks: Notebook[];
  loadError?: string | null;
  kind: ViewKind;
  date: string;
  /** 翻日浏览的固定日期（可为未来日）；null = 跟随今天 */
  dayDate: string | null;
  leftovers: LeftoversDto | null;
  fold: FoldState;
  /** 添加框当前分类（Tab 循环：work → personal → 未分类） */
  addCat: string;
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** "2026-09-10" → "周四"（报头小字）。 */
export function weekdayCn(date: string): string {
  return WEEKDAY_CN[new Date(date + "T12:00:00").getDay()];
}

/** "2026-09-10" → "09.10"（报头大日期，原型 B 格式）。 */
export function mmdd(date: string): string {
  const d = new Date(date + "T12:00:00");
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${mm}.${dd}`;
}

/** 报头右侧 mono 行："2026-W37 · D4"（ISO 周 + 周内序号，周一=1）。 */
export function isoLine(date: string): string {
  const d = new Date(date + "T12:00:00");
  const dow = d.getDay() === 0 ? 7 : d.getDay();
  const thursday = new Date(d);
  thursday.setDate(d.getDate() + (4 - dow));
  const jan1 = new Date(thursday.getFullYear(), 0, 1);
  const week = Math.floor((thursday.getTime() - jan1.getTime()) / 604800000) + 1;
  return `${thursday.getFullYear()}-W${String(week).padStart(2, "0")} · D${dow}`;
}

export function renderNotes(body: HTMLElement, dto: NotesDto): void {
  body.replaceChildren();
  for (const day of dto.kind === "day" ? dto.days.slice(0, 1) : dto.days) {
    const block = el("label", "notes-block");
    block.append(el("span", "notes-date", `${mmdd(day.date)} ${day.weekday}`));
    const input = el("textarea", "notes-input");
    input.value = day.content;
    input.dataset.noteDate = day.date;
    input.dataset.baseVersion = day.base_version;
    input.dataset.loaded = day.content;
    block.append(input);
    body.append(block);
  }
}

function renderTask(task: TaskView, showSource: boolean): HTMLElement {
  const item = el("div", `item ${task.status}`);
  item.dataset.line = String(task.row_idx);

  const prio = task.priority ? PRIO_CLASS[task.priority] : "pnone";
  const bar = el("button", `bar ${prio}`, task.priority ?? "·");
  bar.dataset.action = "prio";
  bar.title = task.priority ?? "无优先级（点击设 P0）";
  bar.setAttribute("aria-label", `切换优先级：${task.priority ?? "未设置"}`);
  item.append(bar);

  // 左侧状态钮（v0.8 四态合一）：单击循环只在进行中⇄暂停；完成态禁点（复活走行尾勾选框）。
  const st = el("button", "st");
  const stMark = el("span", "st-mark", ST_SYMBOL[task.status]);
  stMark.setAttribute("aria-hidden", "true");
  st.append(stMark);
  st.dataset.action = "status";
  st.title = ST_TITLE[task.status];
  st.setAttribute("aria-label", `状态：${STATUS_LABEL[task.status]}`);
  if (task.status === "done") st.setAttribute("aria-disabled", "true");
  item.append(st);

  const body = el("div", "body");
  const line = el("div", "title-line");
  const txt = el("div", "txt");
  txt.textContent = task.display; // 全文自适应，不折叠
  txt.dataset.action = "edit";
  if (!task.checked) {
    txt.tabIndex = 0;
    txt.setAttribute("role", "button");
    txt.setAttribute("aria-label", `编辑：${task.display}`);
    txt.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        txt.click();
      }
    });
  }
  line.append(txt);
  // 待开始不显示计时；一旦离开初始态 00:00:00 也显示（离开即落 #t 0）。
  if (task.status !== "todo") {
    const tm = el("span", "timer", formatHms(timerDisplaySecs(task, Date.now())));
    tm.dataset.timerLine = String(task.row_idx);
    line.append(tm);
  }
  body.append(line);

  // 子行统一容器：编辑时整体隐藏，避免 textarea 与旧子行重复显示。
  if (task.sub_lines.length > 0) {
    const subs = el("div", "subs");
    for (const s of task.sub_lines) subs.append(el("div", "sub-line", s));
    body.append(subs);
  }

  const meta = el("div", "meta");
  if (task.source.kind === "day" && showSource) {
    meta.append(el("span", "source-date", `${mmdd(task.source.date)} ${weekdayCn(task.source.date)}`));
  }
  if (task.overdue) meta.append(el("span", "tag overdue-tag", "逾期"));
  if (meta.childElementCount > 0) body.append(meta);
  item.append(body);

  // 行尾完成勾选框（常显）：勾=完成冻结、取消=恢复为暂停，累计冻结不清零，点状态钮续计。
  const cbx = el("button", "cbx");
  const mark = el("span", "cbx-mark", "✓");
  mark.setAttribute("aria-hidden", "true");
  cbx.append(mark);
  cbx.dataset.action = "check";
  cbx.setAttribute("role", "checkbox");
  cbx.setAttribute("aria-checked", String(task.checked));
  cbx.setAttribute("aria-label", task.checked ? "取消完成并恢复为暂停" : "完成任务");
  cbx.title = task.checked ? "取消勾选：恢复为暂停，累计计时冻结不清零；点状态钮继续计时" : "勾选完成（计时冻结）";
  item.append(cbx);

  const del = el("button", "del");
  del.textContent = "✕";
  del.dataset.action = "del";
  del.title = "删除";
  del.setAttribute("aria-label", `删除：${task.display}`);
  item.append(del);
  return item;
}

function compareTasks(a: TaskView, b: TaskView): number {
  return Number(a.checked) - Number(b.checked)
    || ((a.priority === null ? 4 : PRIORITY_RANK[a.priority])
      - (b.priority === null ? 4 : PRIORITY_RANK[b.priority]));
}

// —— 优先级调档期间的排序冻结 ——
// 连点优先级钮换档时，逐次重排会让目标行每点一次跳一次位、难以继续点击。
// 首次调整时锁定当时的显示序（main.ts holdSortResort 传入），冻结期间渲染
// 沿用锁定序、条目原地换档；停止调整 1.5 秒后解锁并重排一次（时长在 main.ts）。
// 仅影响显示序：文件序、行号引用、同步均不受影响。
let frozenOrder: Map<string, number> | null = null;

/** 跨日/跨视图稳定的任务身份键（与 focusTaskControl 的定位三元组一致）。 */
export function taskKey(t: TaskView): string {
  return `${t.source.kind}:${t.source.date}:${t.line_idx}`;
}

export function holdSortOrder(keys: string[]): void {
  frozenOrder = new Map(keys.map((k, i) => [k, i]));
}

export function releaseSortOrder(): void {
  frozenOrder = null;
}

/** 冻结期按锁定序排（不在锁定内的行视为新增，按常规序排在其后）；否则按常规序排。 */
function orderTasks(tasks: TaskView[]): TaskView[] {
  const rank = frozenOrder;
  if (rank === null) return [...tasks].sort(compareTasks);
  const frozen: TaskView[] = [];
  const fresh: TaskView[] = [];
  for (const t of tasks) (rank.has(taskKey(t)) ? frozen : fresh).push(t);
  frozen.sort((a, b) => rank.get(taskKey(a))! - rank.get(taskKey(b))!);
  return [...frozen, ...fresh.sort(compareTasks)];
}

function renderGroup(cat: Category, tasks: TaskView[], folded: boolean, showSource: boolean): HTMLElement {
  // 显示序≠文件序：完成项全局沉底，两段内部按优先级排序，同段同级保留文件原序
  // （Array.prototype.sort 稳定）。副本排序，不改任务对象与行号引用、不改落盘顺序。
  // 优先级调档冻结期沿用锁定序（见 orderTasks）。
  const ordered = orderTasks(tasks);
  const group = el("section", "group");
  group.dataset.cat = cat;

  const head = el("button", "fold-btn");
  head.dataset.action = "fold";
  head.setAttribute("aria-expanded", String(!folded));
  head.append(el("span", "fold-title", CATEGORY_LABEL[cat]));
  const undone = tasks.filter((t) => !t.checked).length;
  head.append(el("span", "fold-count", `${undone} 项未完成`));
  head.append(el("span", "fold-arrow" + (folded ? "" : " open"), "▸"));
  group.append(head);

  if (!folded) {
    const list = el("div", "list");
    for (const t of ordered) list.append(renderTask(t, showSource));
    group.append(list);
  }
  return group;
}

function renderStar(tasks: TaskView[], showSource: boolean): HTMLElement {
  const sec = el("section", "star-sec");
  sec.append(el("div", "star-title", "⭐ 睡前必须完成"));
  const list = el("div", "list");
  // 与 renderGroup 同序：完成项全局沉底，两段内部按优先级排序，同段同级保留文件原序；
  // 调档冻结期同走 orderTasks 锁定序。
  const ordered = orderTasks(tasks);
  for (const t of ordered) list.append(renderTask(t, showSource));
  sec.append(list);
  return sec;
}

/** F23 本周统计：完成 N/M + 进度条（纯前端由视图计算，week 视图）。 */
function renderStats(root: HTMLElement, kind: ViewKind, tasks: TaskView[]): void {
  const box = root.querySelector<HTMLElement>("#stats")!;
  if (kind !== "week" || tasks.length === 0) {
    box.hidden = true;
    return;
  }
  const done = tasks.filter((t) => t.checked).length;
  const pct = Math.round((done / tasks.length) * 100);
  box.hidden = false;
  box.replaceChildren();
  box.append(el("span", undefined, `本周 ${done}/${tasks.length} 完成`));
  const bar = el("div", "stats-bar");
  const fill = el("div", "stats-fill");
  fill.style.width = `${pct}%`;
  bar.append(fill);
  box.append(bar, el("span", "stats-pct", `${pct}%`));
}

/** F14 提示条 + 逾期聚合（day 视图）。 */
function renderCarry(
  root: HTMLElement,
  leftovers: LeftoversDto | null,
  open: boolean,
): void {
  const carry = root.querySelector<HTMLElement>("#carry")!;
  if (!leftovers || leftovers.count === 0) {
    carry.hidden = true;
    return;
  }
  carry.hidden = false;
  carry.replaceChildren();

  const line = el("div", "carry-line");
  line.textContent = `昨天还有 ${leftovers.count} 项未完成`;
  const btn = el("button", "carry-btn");
  btn.textContent = open ? "收起" : "复制到今天";
  btn.dataset.action = "carry-toggle";
  line.append(btn);
  carry.append(line);

  if (open) {
    for (const t of leftovers.tasks) {
      const row = el("div", "carry-row");
      row.textContent = t.content;
      const copy = el("button", "carry-copy");
      copy.textContent = "＋";
      copy.title = "复制到今天";
      copy.dataset.action = "carry-copy";
      copy.dataset.line = String(t.line_idx);
      row.append(copy);
      carry.append(row);
    }
  }
}

function renderOverdueSum(root: HTMLElement, tasks: TaskView[]): void {
  const sum = root.querySelector<HTMLElement>("#overdue-sum")!;
  const n = tasks.filter((t) => !t.checked && t.overdue).length;
  if (n === 0) {
    sum.hidden = true;
    return;
  }
  sum.hidden = false;
  sum.replaceChildren();
  const span = el("span", undefined, `${n} 项标注了 #overdue`);
  const note = el("span", "sum-note", "（只读）");
  sum.append(span, note);
}

/** 整体重渲（v1 不做 keyed-diff；列表规模 ~几十，重渲足够快且无输入态冲突）。 */
export function render(
  root: HTMLElement,
  state: UiState,
  view: { date: string; relPath: string; tasks: TaskView[] } | null,
): void {
  root.querySelector<HTMLElement>("#head-weekday")!.textContent = view
    ? weekdayCn(view.date)
    : "—";
  root.querySelector<HTMLElement>("#head-date")!.textContent = view
    ? mmdd(view.date)
    : "--.--";
  root.querySelector<HTMLElement>("#head-iso")!.textContent = view
    ? isoLine(view.date)
    : "";

  // 翻日导航：仅日视图显示；‹ › 恒显（可向前也可向未来），
  // 「回今天」在固定浏览某日（历史或未来）时才出现
  const isDay = state.kind === "day";
  root.querySelector<HTMLElement>("#nav-prev")!.hidden = !isDay;
  root.querySelector<HTMLElement>("#nav-next")!.hidden = !isDay;
  root.querySelector<HTMLElement>("#head-today")!.hidden = !isDay || state.dayDate === null;
  if (TOUCH_DEVICE) {
    const today = root.querySelector<HTMLElement>("#head-today")!;
    today.textContent = "今";
    today.setAttribute("aria-label", "回今天");
  }

  for (const k of ["day", "week"] as ViewKind[]) {
    root.querySelector<HTMLElement>(`#tab-${k}`!)?.classList.toggle(
      "active",
      state.kind === k,
    );
    root.querySelector<HTMLElement>(`#tab-${k}`)?.setAttribute(
      "aria-selected",
      String(state.kind === k),
    );
  }

  // F14 遗留条只属于跟随今天的日视图（翻历史/未来日时无「昨天遗留」语义）
  renderCarry(root, state.kind === "day" && !state.dayDate ? state.leftovers : null, state.fold.overdueOpen);
  renderStats(root, state.kind, view?.tasks ?? []);

  const groups = root.querySelector<HTMLElement>("#groups")!;
  groups.replaceChildren();
  renderOverdueSum(root, view?.tasks ?? []);

  if (TOUCH_DEVICE && state.loadError) {
    const failure = el("div", "empty load-error");
    failure.setAttribute("role", "alert");
    const retry = el("button", "load-retry", "重试");
    retry.type = "button";
    retry.dataset.action = "retry-load";
    failure.append(el("p", undefined, state.loadError), retry);
    groups.append(failure);
  } else if (TOUCH_DEVICE && view && view.tasks.length === 0) {
    const isToday = isDay && state.dayDate === null;
    const empty = el("div", "empty touch-empty",
      !isDay ? "本周暂无事项" : isToday ? "今天还没有事项 · 点右下角记一条" : "这一天没有记录");
    if (isToday) {
      const cue = el("span", "empty-fab-cue", "↘");
      cue.setAttribute("aria-hidden", "true");
      empty.append(cue);
    }
    groups.append(empty);
  } else if (!view || view.tasks.length === 0) {
    groups.append(
      el(
        "div",
        "empty",
        !view ? "正在加载事项…" : state.kind === "week" ? "本周还没有事项，记一条吧" : state.dayDate ? "这天没有记录" : "今天还没有事项，记一条吧",
      ),
    );
  } else {
    // ⭐ 子区条目单独归入 star-sec（保留原顺序），其余按分类三组
    const star = view.tasks.filter((t) => t.subsection !== null);
    const rest = view.tasks.filter((t) => t.subsection === null);
    for (const cat of CATEGORY_ORDER) {
      const inCat = rest.filter((t) => t.category === cat);
      if (inCat.length === 0) continue;
      groups.append(renderGroup(cat, inCat, state.fold.folded.has(cat), !isDay));
    }
    if (star.length > 0) groups.append(renderStar(star, !isDay));
  }

  if (view && view.tasks.length === 0 && !state.loadError) {
    const others = state.notebooks.filter(book => book.id !== state.notebookId);
    if (others.length > 0) {
      const hint = el("div", "empty-notebooks", "其他笔记本可能有你的事项：");
      others.forEach((book, index) => {
        if (index > 0) hint.append("、");
        const button = el("button", "empty-notebook-link", book.name);
        button.type = "button";
        button.dataset.action = "switch-notebook";
        button.dataset.notebookId = book.id;
        hint.append(button);
      });
      groups.querySelector(".empty")?.append(hint);
    }
  }

  renderAdd(root, state);
}

/** 添加入口（.add 按钮 / 展开输入框）。 */
export function renderAdd(root: HTMLElement, state: { addCat: string }): void {
  const slot = root.querySelector<HTMLElement>("#add-slot")!;
  slot.replaceChildren();
  const btn = el("button", "add");
  btn.append("＋ 记一条…", el("span", "desktop-hint", `（Enter 落 [${state.addCat}]，Tab 切分类）`));
  if (TOUCH_DEVICE) {
    btn.classList.add("add-fab");
    btn.setAttribute("aria-label", btn.textContent!);
  }
  btn.dataset.action = "add";
  slot.append(btn);
}

/** 同步角标。 */
export function renderSync(root: HTMLElement, s: SyncState | null): void {
  const dot = root.querySelector<HTMLElement>("#sync-dot")!;
  const text = root.querySelector<HTMLElement>("#sync-text")!;
  text.removeAttribute("title");
  dot.dataset.state = s?.kind ?? "idle";
  switch (s?.kind) {
    case "syncing":
      text.textContent = "同步中…";
      break;
    case "offline":
      text.textContent = `离线 · ${s.unpushed} 条待推`;
      break;
    case "conflict":
      text.textContent = "冲突：需人工处理";
      text.title = s.message;
      break;
    case "error":
      text.textContent = "同步异常";
      text.title = s.message;
      break;
    default:
      text.textContent = "已同步";
  }
}
