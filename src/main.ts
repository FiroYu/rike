/**
 * 贴纸主逻辑：视图状态 + 事件委托 + 5s 沉回 / Esc + 高度自适应。
 *
 * 编辑使用开始操作时的视图/版本；后台刷新延迟到输入完成，避免拆除草稿。
 */

import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  ApiError,
  api,
  type DeleteResult,
  type Notebook,
  type TaskView,
  type SyncState,
  type ViewDto,
  type ViewKind,
} from "./api";
import { render, renderAdd, renderSync, renderSyncSettings, renderNotes, mmdd, weekdayCn, isoLine, TOUCH_DEVICE, formatHms, timerDisplaySecs, TIMER_CAP_SECS, taskKey, holdSortOrder, releaseSortOrder, type FoldState } from "./render";

const SINK_IDLE_MS = 5000;
const ADD_CAT_CYCLE: Array<string | null> = ["work", "personal", null];
const ADD_CAT_LABEL: Record<string, string> = {
  work: "工作",
  personal: "个人",
  "": "未分类",
};

interface App {
  notebookId: string;
  notebooks: Notebook[];
  kind: ViewKind;
  /** 翻日浏览的目标日期（YYYY-MM-DD），可为未来日；null = 跟随今天 */
  dayDate: string | null;
  view: ViewDto | null;
  repoReady: boolean;
  addCatIdx: number;
  fold: FoldState;
}

function savedNotebook(): string {
  try { return localStorage.getItem("sticky.notebook") || "default"; } catch { return "default"; }
}

const app: App = {
  notebookId: savedNotebook(),
  notebooks: [],
  kind: "day",
  dayDate: null,
  view: null,
  repoReady: false,
  addCatIdx: 0,
  fold: { folded: new Set(), overdueOpen: false },
};

const root = document.querySelector<HTMLElement>("#sticky")!;
if (TOUCH_DEVICE) {
  root.querySelectorAll("[data-tauri-drag-region]").forEach(node => node.removeAttribute("data-tauri-drag-region"));
  const viewport = window.visualViewport;
  if (viewport) {
    const updateViewport = () => {
      const inset = Math.max(0, window.innerHeight - viewport.height - viewport.offsetTop);
      root.style.setProperty("--touch-viewport-inset", `${inset}px`);
      root.style.setProperty("--touch-viewport-height", `${viewport.height}px`);
    };
    viewport.addEventListener("resize", updateViewport);
    viewport.addEventListener("scroll", updateViewport);
    updateViewport();
  }
}

interface EditContext {
  notebookId: string; viewKey: string; kind: ViewKind; date: string; baseVersion: string; lineIdx: number;
}
let refreshId = 0;
let loadError: string | null = null;
let mutationPending = false;
let refreshDeferred = false;
// v0.8 多行编辑：录入/编辑均为 textarea（Shift+回车换行）
let editor: { input: HTMLInputElement | HTMLTextAreaElement; cancel: () => void } | null = null;
const errorMessage = document.createElement("div");
errorMessage.setAttribute("role", "alert");
errorMessage.style.cssText = "padding:8px 16px;color:var(--ink, #442b28);font-size:12px;white-space:pre-wrap";
errorMessage.hidden = true;
root.append(errorMessage);

function reportError(error: unknown): void {
  errorMessage.textContent = error instanceof ApiError && error.code === "stale"
    ? editor ? "事项已被其他操作更新。草稿已保留，请复制后按 Esc 退出并重新编辑。" : "事项已被其他操作更新，正在刷新，请重试。"
    : `操作失败：${error instanceof Error ? error.message : String(error)}。请重试。`;
  errorMessage.hidden = false;
}

function viewKey(): string { return `${app.notebookId}/${app.kind}/${app.dayDate ?? todayStr()}`; }

function context(row?: number): EditContext | null {
  if (!app.view || mutationPending) return null;
  const task = row === undefined ? undefined : taskAt(row);
  if (row !== undefined && !task) return null;
  const source = task?.source ?? app.view.sources[0];
  return { notebookId: app.notebookId, viewKey: viewKey(), kind: source.kind, date: source.date,
    baseVersion: source.base_version, lineIdx: task?.line_idx ?? -1 };
}

function isCurrent(c: EditContext): boolean { return c.viewKey === viewKey(); }

function sourceVersion(c: EditContext, view = app.view): string | undefined {
  return view?.sources.find(s => s.kind === c.kind && s.date === c.date)?.base_version;
}

function finishEditor(): void {
  editor = null;
  errorMessage.hidden = true;
  if (refreshDeferred) {
    refreshDeferred = false;
    void refresh();
  }
}

function canNavigate(): boolean {
  const note = dirtyNotes()[0] ?? (TOUCH_DEVICE ? notesBody.querySelector<HTMLTextAreaElement>('[data-saving="true"]') : noteSquare.dataset.saving === "true" ? noteSquare : null);
  if (note) {
    if (TOUCH_DEVICE) {
      notesPanel.hidden = false;
      notesToggle.setAttribute("aria-expanded", "true");
    }
    notesStatus.textContent = "速记草稿尚未保存，请等自动保存完成或手动收起";
    note.focus();
    return false;
  }
  if (!editor) return true;
  if (editor.input.value.trim()) {
    errorMessage.textContent = "草稿尚未保存，请按 Enter 保存或 Esc 取消后切换。";
    errorMessage.hidden = false;
    editor.input.focus();
    return false;
  }
  editor.cancel();
  return true;
}

function navigate(): void {
  hideUndo();
  if (!TOUCH_DEVICE) void loadNoteSquare();
  app.view = null;
  rerender();
  void refresh();
  if (!notesPanel.hidden && !dirtyNotes().length) void loadNotes();
}

/** Date → 本地 YYYY-MM-DD（翻日导航用）。 */
function fmtDate(d: Date): string {
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${mm}-${dd}`;
}

function todayStr(): string {
  return fmtDate(new Date());
}

function uiState() {
  const cat = ADD_CAT_CYCLE[app.addCatIdx];
  return {
    loadError,
    notebookId: app.notebookId,
    notebooks: app.notebooks,
    kind: app.kind,
    date: app.view?.date ?? "",
    dayDate: app.dayDate,
    leftovers: leftoversCache,
    fold: app.fold,
    addCat: ADD_CAT_LABEL[cat ?? ""] ?? "未分类",
  };
}

let leftoversCache: Awaited<ReturnType<typeof api.getLeftovers>> | null = null;

function rerender(): void {
  if (editor) return;
  closeCtx(); // 行号可能因后台 pull 变化，菜单持有的捕获上下文即刻失效
  render(
    root,
    uiState(),
    app.repoReady && app.view
      ? {
          date: app.view.date,
          relPath: app.view.rel_path,
          tasks: app.view.tasks,
        }
      : null,
  );
  syncTicker();
  requestAnimationFrame(measureHeight);
}

/** 成功写入后旧行号立即失效；完整视图返回前不允许旧任务搭配新版本写入。 */
async function applyResult(c: EditContext): Promise<void> {
  if (!isCurrent(c)) return;
  app.view = null;
  rerender();
  await refresh();
}

/** 拉当前视图 + 昨日遗留；成功即视为仓库就绪（repo-ready 事件可能在
 *  listen 注册前已发出——本地克隆快于 JS 加载，事件会丢）。 */
async function refresh(silent = true): Promise<void> {
  if (editor || mutationPending) {
    refreshDeferred = true;
    return;
  }
  const id = ++refreshId;
  if (TOUCH_DEVICE && loadError) { loadError = null; rerender(); }
  const kind = app.kind;
  const date = app.dayDate;
  const notebookId = app.notebookId;
  try {
    const [v, lo, notebooks] = await Promise.all([
      api.getView(notebookId, kind, date),
      api.getLeftovers(notebookId).catch(() => null),
      api.listNotebooks(),
    ]);
    if (id !== refreshId || kind !== app.kind || date !== app.dayDate || notebookId !== app.notebookId) return;
    if (editor || mutationPending) { refreshDeferred = true; return; }
    if (undoPending && undoPending.baseVersion !== sourceVersion(undoPending, v)) hideUndo();
    app.view = v;
    leftoversCache = lo;
    app.notebooks = notebooks;
    renderNotebooks();
    if (!TOUCH_DEVICE) void loadNoteSquare();
    app.repoReady = true;
    loadError = null;
    rerender();
  } catch (e) {
    // 本机记忆的笔记本可能已不存在（他机删除、仓库重置后悬空）：
    // 名单里查无此本就回到默认笔记本，而不是卡死在加载报错。
    if (id === refreshId && !app.view) {
      try {
        const books = await api.listNotebooks();
        if (id !== refreshId) return;
        if (!books.some(b => b.id === notebookId)) {
          app.notebooks = books;
          switchNotebook("default");
          return;
        }
      } catch { /* 名单也拉不到：走下方既有错误路径 */ }
    }
    if (TOUCH_DEVICE && id === refreshId) {
      const message = e && typeof e === "object" && "message" in e ? e.message : e;
      loadError = String(message || "无法加载事项或笔记本，请重试").slice(0, 120);
      rerender();
      return;
    }
    // Startup validation/clone can still be in progress; the loading view and
    // sync status already describe it, so do not leave a false edit-error banner.
    if (id === refreshId && !app.view && !(e instanceof ApiError && e.code === "repo_not_ready")) reportError(e);
    if (!silent) console.error("刷新失败", e);
  }
}

async function edit(fn: (c: EditContext) => Promise<{ base_version: string }>, c = context()): Promise<boolean> {
  if (!c || mutationPending) return false;
  mutationPending = true;
  ++refreshId;
  hideUndo();
  errorMessage.hidden = true;
  try {
    await fn(c);
    mutationPending = false;
    await applyResult(c);
    return true;
  } catch (e) {
    mutationPending = false;
    reportError(e);
    if (e instanceof ApiError && e.code === "stale") {
      await refresh();
    }
    return false;
  } finally {
    mutationPending = false;
    if (refreshDeferred && !editor) { refreshDeferred = false; void refresh(); }
  }
}

function taskAt(line: number): TaskView | undefined {
  return app.view?.tasks[line];
}

// ---- v0.8 计时 ticker（§2.8）----
// 每秒从累计值 + 时间戳重算（非 counter++）；只改 Text 节点 data，不 render/refresh/IPC；
// 存在未封顶的有效 Doing 且页面可见时才运行 interval；重渲后按 DOM 重新定位。

let tickerId: number | undefined;

function tickTimers(): void {
  const timers = root.querySelectorAll<HTMLElement>(".timer[data-timer-line]");
  if (timers.length === 0) return;
  const now = Date.now();
  for (const tm of timers) {
    const t = app.view?.tasks[Number(tm.dataset.timerLine)];
    if (!t) continue;
    const text = formatHms(timerDisplaySecs(t, now));
    const first = tm.firstChild;
    if (first instanceof Text && first.data !== text) first.data = text;
  }
}

function hasTickingTimer(): boolean {
  const now = Date.now();
  return !!app.view?.tasks.some(t =>
    t.status === "doing" && t.timer_started_at !== null && timerDisplaySecs(t, now) < TIMER_CAP_SECS);
}

function syncTicker(): void {
  const active = !document.hidden && hasTickingTimer();
  if (active && tickerId === undefined) {
    tickerId = window.setInterval(tickTimers, 1000);
  } else if (!active && tickerId !== undefined) {
    window.clearInterval(tickerId);
    tickerId = undefined;
  }
  if (!document.hidden) tickTimers(); // 含从后台标签恢复时的立即重算
}

document.addEventListener("visibilitychange", syncTicker);

/** 完成沉底/状态切换重排后，把焦点恢复到同一来源任务的指定控件（不能拿重排前 row_idx）。 */
function focusTaskControl(t: TaskView, selector: string): void {
  const idx = app.view?.tasks.findIndex(x =>
    x.line_idx === t.line_idx && x.source.kind === t.source.kind && x.source.date === t.source.date);
  if (idx === undefined || idx < 0) return;
  root.querySelector<HTMLElement>(`.item[data-line="${idx}"] ${selector}`)?.focus();
}

// Notebook requests carry their own ID; switching cannot redirect a pending write.
const notebookNav = root.querySelector<HTMLElement>(".notebook-nav")!;
const notebookToggle = root.querySelector<HTMLButtonElement>("#notebook-toggle")!;
const notebookPanel = root.querySelector<HTMLElement>("#notebook-panel")!;
const notebookForm = root.querySelector<HTMLFormElement>("#notebook-form")!;
const notebookName = root.querySelector<HTMLInputElement>("#notebook-name")!;
const notebookError = root.querySelector<HTMLElement>("#notebook-error")!;
const appearance = root.querySelector<HTMLDetailsElement>("#appearance")!;
let renaming: Notebook | null = null;
let notebookSaving = false;

renderSyncSettings(notebookPanel);

const notesNav = root.querySelector<HTMLElement>(".notes-nav")!;
const notesToggle = root.querySelector<HTMLButtonElement>("#notes-toggle")!;
const notesPanel = root.querySelector<HTMLElement>("#notes-panel")!;
const notesBody = root.querySelector<HTMLElement>("#notes-body")!;
const notesTitle = root.querySelector<HTMLElement>("#notes-title")!;
const notesStatus = root.querySelector<HTMLElement>(TOUCH_DEVICE ? "#notes-status" : "#note-square-status")!;
const noteSquare = root.querySelector<HTMLTextAreaElement>("#note-square textarea")!;
let notesRequestId = 0;

function dirtyNotes(): HTMLTextAreaElement[] {
  // 桌面与触摸同口径：常驻方块与速记面板的输入框一律计入草稿判定，
  // 避免面板草稿被后台刷新（view-changed）或导航重绘清掉。
  return [noteSquare, ...Array.from(notesBody.querySelectorAll<HTMLTextAreaElement>(".notes-input"))]
    .filter(input => input.value !== input.dataset.loaded);
}

const NOTE_CACHE_KEY = "sticky.note.today";
let squareRequestId = 0;
let squareLoading: Promise<void> | null = null;

function cacheNoteSquare(date: string, content: string, notebookId: string): void {
  try {
    // Keep the requested cache shape, with a separate owner to avoid notebook leaks.
    localStorage.setItem(NOTE_CACHE_KEY + ".notebook", notebookId);
    localStorage.setItem(NOTE_CACHE_KEY, JSON.stringify({ date, content }));
  } catch { /* Storage is optional. */ }
}

function loadNoteSquare(): Promise<void> {
  const notebookId = app.notebookId;
  const date = todayStr();
  const changed = noteSquare.dataset.notebookId !== notebookId || noteSquare.dataset.noteDate !== date;
  if (changed) {
    if (dirtyNotes().length || noteSquare.dataset.saving === "true") return Promise.resolve();
    noteSquare.value = "";
    noteSquare.dataset.loaded = "";
    noteSquare.dataset.notebookId = notebookId;
    noteSquare.dataset.noteDate = date;
    delete noteSquare.dataset.baseVersion;
    notesStatus.textContent = "";
    try {
      const cached = JSON.parse(localStorage.getItem(NOTE_CACHE_KEY) || "null");
      const owner = localStorage.getItem(NOTE_CACHE_KEY + ".notebook");
      if (cached?.date === date && typeof cached.content === "string" && (!owner || owner === notebookId)) {
        noteSquare.value = cached.content;
        noteSquare.dataset.loaded = cached.content;
      }
    } catch { /* Ignore invalid cache. */ }
  } else if (squareLoading) return squareLoading;
  else if (noteSquare.dataset.saving === "true") return Promise.resolve();
  const id = ++squareRequestId;
  squareLoading = (async () => {
    try {
      const dto = await api.getNotes(notebookId, "day", null);
      if (id !== squareRequestId || notebookId !== app.notebookId) return;
      const day = dto.days.find(day => day.date === date);
      if (!day) return;
      if (noteSquare.value !== noteSquare.dataset.loaded) return;
      if (noteSquare.value && noteSquare.value !== day.content) {
        // 关窗兜底草稿与远端不同：保留草稿并视为未保存，交给正常保存流程落盘，
        // 不被远端内容静默顶掉；本地兜底缓存同样保留，待保存成功后再刷新。
        noteSquare.dataset.loaded = day.content;
        noteSquare.dataset.baseVersion = day.base_version;
        notesStatus.textContent = "已恢复未保存的速记草稿，失焦保存将覆盖远端";
        return;
      }
      if (noteSquare.value !== day.content) noteSquare.value = day.content;
      noteSquare.dataset.loaded = day.content;
      noteSquare.dataset.baseVersion = day.base_version;
      cacheNoteSquare(date, day.content, notebookId);
      notesStatus.textContent = "";
    } catch (error) {
      if (id === squareRequestId) notesStatus.textContent = error instanceof Error ? error.message : String(error);
    } finally {
      if (id === squareRequestId) squareLoading = null;
    }
  })();
  return squareLoading;
}

noteSquare.dataset.loaded = ""; // 触摸端方块隐藏，但同样参与 dirtyNotes 判定，需先初始化口径
if (!TOUCH_DEVICE) {
  noteSquare.addEventListener("blur", () => { void saveNote(noteSquare); });
  root.querySelector("#note-square")!.addEventListener("mouseleave", () => { void saveNote(noteSquare); });
  void loadNoteSquare();
}

async function loadNotes(): Promise<void> {
  if (dirtyNotes().length || notesBody.querySelector('[data-saving="true"]')) return;
  const id = ++notesRequestId;
  const key = viewKey();
  const notebookId = app.notebookId;
  const kind = app.kind;
  const date = app.view?.date ?? app.dayDate ?? todayStr();
  notesTitle.textContent = kind === "day" ? `速记 · ${mmdd(date)} ${weekdayCn(date)}`
    : `速记 · ${isoLine(date).split(" · ")[0]}`;
  notesBody.replaceChildren();
  notesStatus.textContent = "正在加载…";
  try {
    const dto = await api.getNotes(notebookId, kind, app.dayDate);
    if (id !== notesRequestId || key !== viewKey() || notesPanel.hidden || dirtyNotes().length) return;
    if (kind === "day") dto.days.forEach(day => { day.weekday = weekdayCn(date); });
    renderNotes(notesBody, dto);
    notesBody.querySelectorAll<HTMLTextAreaElement>(".notes-input").forEach(input => {
      input.dataset.notebookId = notebookId;
    });
    notesStatus.textContent = "";
  } catch (error) {
    if (id === notesRequestId) notesStatus.textContent = error instanceof Error ? error.message : String(error);
  }
}

async function saveNote(input: HTMLTextAreaElement): Promise<boolean> {
  if (!TOUCH_DEVICE && input === noteSquare && (squareLoading || !input.dataset.baseVersion)) {
    await loadNoteSquare();
    if (!input.dataset.baseVersion) return false;
  }
  // Blur and collapse can arrive together; serialize writes for each day.
  if (input.dataset.saving === "true") {
    if (input.value !== input.dataset.savingContent) input.dataset.saveAgain = "true";
    return false;
  }
  if (input.value === input.dataset.loaded) return true;
  const content = input.value;
  if (new TextEncoder().encode(content).byteLength > 64000) {
    notesStatus.textContent = "速记内容超过 64000 字节，请缩短后保存";
    return false;
  }
  const notebookId = input.dataset.notebookId!;
  const date = input.dataset.noteDate!;
  input.dataset.saving = "true";
  input.dataset.savingContent = content;
  notesStatus.textContent = "正在保存…";
  let saved = false;
  try {
    const result = await api.saveNote(notebookId, date, content, input.dataset.baseVersion!);
    input.dataset.baseVersion = result.base_version;
    input.dataset.loaded = content;
    if (!TOUCH_DEVICE && input === noteSquare) cacheNoteSquare(date, content, notebookId);
    notesStatus.textContent = input.value === content ? "已保存" : "仍有未保存的修改";
    saved = true;
  } catch (error) {
    if (error instanceof ApiError && error.code === "stale") {
      try {
        const dto = await api.getNotes(notebookId, "day", date);
        input.dataset.baseVersion = dto.days.find(day => day.date === date)?.base_version ?? "0";
        notesStatus.textContent = "远端有更新，版本已刷新；再次保存将覆盖远端";
      } catch (reloadError) {
        notesStatus.textContent = reloadError instanceof Error ? reloadError.message : String(reloadError);
      }
    } else {
      notesStatus.textContent = error instanceof Error ? error.message : String(error);
    }
  } finally {
    const again = input.dataset.saveAgain === "true";
    delete input.dataset.saving;
    delete input.dataset.savingContent;
    delete input.dataset.saveAgain;
    if (saved && again) void saveNote(input);
  }
  return saved;
}

function closeNotes(restoreFocus = true): void {
  ++notesRequestId;
  dirtyNotes().forEach(input => { void saveNote(input); });
  notesPanel.hidden = true;
  notesToggle.setAttribute("aria-expanded", "false");
  if (restoreFocus) notesToggle.focus();
}

notesToggle.addEventListener("click", () => {
  if (!notesPanel.hidden) { closeNotes(); return; }
  // A failed save remains in the hidden panel and must survive reopening.
  if (!canNavigate() || notebookSaving) return;
  closeNotebooks(false);
  appearance.open = false;
  notesPanel.hidden = false;
  notesToggle.setAttribute("aria-expanded", "true");
  root.querySelector<HTMLButtonElement>("#notes-close")!.focus();
  void loadNotes();
});
root.querySelector("#notes-close")!.addEventListener("click", () => closeNotes());
notesBody.addEventListener("input", ev => {
  if (!(ev.target instanceof HTMLTextAreaElement)) return;
  ++notesRequestId;
});
notesBody.addEventListener("blur", ev => {
  if (ev.target instanceof HTMLTextAreaElement) void saveNote(ev.target);
}, true);
notesBody.addEventListener("keydown", ev => {
  if (ev.isComposing || ev.keyCode === 229 || ev.key !== "Escape") return;
  ev.preventDefault();
  ev.stopPropagation();
  closeNotes();
});
window.addEventListener("click", ev => {
  if (!notesPanel.hidden && !notesNav.contains(ev.target as Node)) {
    closeNotes(notesPanel.contains(document.activeElement));
  }
}, true);
const syncSettingsToggle = root.querySelector<HTMLButtonElement>("#sync-settings-toggle")!;
const syncSettingsForm = root.querySelector<HTMLFormElement>("#sync-settings-form")!;
const syncPat = root.querySelector<HTMLInputElement>("#sync-pat")!;
const syncSettingsStatus = root.querySelector<HTMLElement>("#sync-settings-status")!;
const syncPatSet = root.querySelector<HTMLElement>("#sync-pat-set")!;
let syncSettingsBusy = false;

syncSettingsToggle.addEventListener("click", () => {
  if (syncSettingsBusy) return;
  syncSettingsForm.hidden = !syncSettingsForm.hidden;
  syncSettingsToggle.setAttribute("aria-expanded", String(!syncSettingsForm.hidden));
  syncPat.value = "";
  if (syncSettingsForm.hidden) return;
  syncSettingsStatus.textContent = "读取中…";
  void api.getSyncConfig().then(config => {
    root.querySelector<HTMLInputElement>("#sync-repo-url")!.value = config.repo_url;
    syncPatSet.hidden = !config.pat_set;
    syncSettingsStatus.textContent = "";
    if (!syncSettingsForm.hidden) syncPat.focus();
  }).catch(() => { syncSettingsStatus.textContent = "无法读取同步设置，请重试"; });
});

// Register before requesting sync; an accepted request alone is not success.
async function testSyncConnection(): Promise<void> {
  let started = false;
  let finished = false;
  let timeout: number | undefined;
  let stop: (() => void) | undefined;
  try {
    await new Promise<void>((resolve, reject) => {
      timeout = window.setTimeout(() => reject(new Error("同步失败: 等待同步结果超时")), 90_000);
      void listen<SyncState>("sync-status", ({ payload }) => {
        if (payload.kind === "syncing") started = true;
        else if (payload.kind === "idle" && started) resolve();
        else if (payload.kind === "conflict" || payload.kind === "error") reject(new Error(payload.message));
        else if (payload.kind === "offline") reject(new Error("同步失败: 当前离线"));
      }).then(unlisten => {
        if (finished) { unlisten(); return; }
        stop = unlisten;
        return api.syncNow();
      }).catch(reject);
    });
  } finally {
    finished = true;
    window.clearTimeout(timeout);
    stop?.();
  }
}

async function saveSyncSettings(test: boolean): Promise<void> {
  if (syncSettingsBusy) return;
  syncSettingsBusy = true;
  const fields = Array.from(syncSettingsForm.querySelectorAll<HTMLInputElement | HTMLButtonElement>("input,button"));
  fields.forEach(field => { field.disabled = true; });
  syncSettingsStatus.textContent = test ? "正在测试同步…" : "正在保存…";
  let saved = false;
  try {
    // Blank means preserve the stored PAT; never populate this input from storage.
    const pat = syncPat.value.trim();
    syncPat.value = "";
    await api.setSyncConfig(undefined, pat || undefined);
    saved = true;
    if (pat) syncPatSet.hidden = false;
    if (test) await testSyncConnection();
    syncSettingsStatus.textContent = test ? "同步正常" : "已保存";
  } catch (error) {
    syncSettingsStatus.textContent = saved
      ? (error instanceof Error ? error.message : "同步失败: 无法请求同步，请重试")
      : "保存失败，请重新输入 PAT 后重试";
  } finally {
    syncSettingsBusy = false;
    fields.forEach(field => { field.disabled = false; });
  }
}

syncSettingsForm.addEventListener("submit", event => {
  event.preventDefault();
  void saveSyncSettings(false);
});
root.querySelector("#sync-test")!.addEventListener("click", () => { void saveSyncSettings(true); });

function closeNotebooks(restoreFocus = true): void {
  if (notebookSaving) return;
  notebookPanel.hidden = true;
  notebookForm.hidden = true;
  syncSettingsForm.hidden = true;
  syncPat.value = "";
  syncSettingsToggle.setAttribute("aria-expanded", "false");
  notebookToggle.setAttribute("aria-expanded", "false");
  if (restoreFocus) notebookToggle.focus();
}

function switchNotebook(notebookId: string): void {
  if (notebookSaving || !canNavigate()) return;
  closeNotes(false);
  app.notebookId = notebookId;
  try { localStorage.setItem("sticky.notebook", notebookId); } catch { /* usable without preference storage */ }
  leftoversCache = null;
  app.fold = { folded: new Set(), overdueOpen: false };
  app.addCatIdx = 0;
  closeNotebooks();
  renderNotebooks();
  navigate();
}

function renderNotebooks(): void {
  const selected = app.notebooks.find(n => n.id === app.notebookId);
  root.querySelector("#notebook-label")!.textContent = selected?.name ?? "笔记本";
  notebookToggle.title = selected ? `切换笔记本 · ${selected.name}` : "切换笔记本";
  const list = root.querySelector("#notebook-list")!;
  const focusedId = list.contains(document.activeElement)
    ? (document.activeElement as HTMLButtonElement).dataset.notebookId : undefined;
  list.replaceChildren(...app.notebooks.map(book => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "notebook-option";
    button.textContent = book.name;
    button.title = book.name;
    button.dataset.notebookId = book.id;
    button.setAttribute("aria-pressed", String(book.id === app.notebookId));
    button.addEventListener("click", () => switchNotebook(book.id));
    return button;
  }));
  if (focusedId && !notebookPanel.hidden) {
    (Array.from(list.querySelectorAll("button")).find(button => button.dataset.notebookId === focusedId)
      ?? notebookToggle).focus();
  }
}

notebookToggle.addEventListener("click", () => {
  if (!canNavigate() || notebookSaving) return;
  if (!notebookPanel.hidden) { closeNotebooks(); return; }
  appearance.open = false;
  notebookPanel.hidden = false;
  notebookToggle.setAttribute("aria-expanded", "true");
  renderNotebooks();
  notebookPanel.querySelector<HTMLButtonElement>('.notebook-option[aria-pressed="true"], #notebook-create')!.focus();
  if (app.notebooks.length === 0) void refresh();
});

// Collapse after the click target is settled so the footer does not move before pointerup.
window.addEventListener("click", ev => {
  if (!notebookPanel.hidden && !notebookNav.contains(ev.target as Node)) {
    closeNotebooks(notebookPanel.contains(document.activeElement));
  }
}, true);

appearance.addEventListener("toggle", () => {
  if (appearance.open && !notebookPanel.hidden) {
    if (notebookSaving) appearance.open = false;
    else closeNotebooks(false);
  }
});

for (const mode of ["create", "rename"] as const) {
  root.querySelector(`#notebook-${mode}`)!.addEventListener("click", () => {
    if (notebookSaving) return;
    renaming = mode === "rename" ? app.notebooks.find(n => n.id === app.notebookId) ?? null : null;
    if (mode === "rename" && !renaming) return;
    root.querySelector("#notebook-form-label")!.textContent = renaming ? "重命名笔记本" : "新建笔记本";
    notebookForm.hidden = false;
    notebookError.hidden = true;
    notebookName.value = renaming?.name ?? "";
    notebookName.focus();
    notebookName.select();
  });
}
root.querySelector("#notebook-cancel")!.addEventListener("click", () => {
  if (notebookSaving) return;
  notebookForm.hidden = true;
  root.querySelector<HTMLButtonElement>(renaming ? "#notebook-rename" : "#notebook-create")!.focus();
});
notebookForm.addEventListener("submit", ev => {
  ev.preventDefault();
  if (notebookSaving || !notebookName.value.trim()) return;
  if (!TOUCH_DEVICE && !canNavigate()) return;
  notebookSaving = true;
  const fields = Array.from(notebookForm.querySelectorAll<HTMLInputElement | HTMLButtonElement>("input,button"));
  fields.forEach(f => f.disabled = true);
  const request = renaming ? api.renameNotebook(renaming.id, notebookName.value, renaming.base_version)
    : api.createNotebook(notebookName.value);
  void request.then(async book => {
    app.notebookId = book.id;
    try { localStorage.setItem("sticky.notebook", book.id); } catch { /* local preference only */ }
    notebookSaving = false;
    closeNotebooks();
    leftoversCache = null;
    app.fold = { folded: new Set(), overdueOpen: false };
    navigate();
  }).catch(error => {
    notebookError.textContent = error instanceof Error ? error.message : String(error);
    notebookError.hidden = false;
  }).finally(() => {
    notebookSaving = false;
    fields.forEach(f => f.disabled = false);
    if (!notebookForm.hidden && (document.activeElement === document.body || notebookPanel.contains(document.activeElement))) {
      notebookName.focus();
    }
  });
});
notebookName.addEventListener("keydown", ev => {
  if ((ev.isComposing || ev.keyCode === 229) && ev.key === "Enter") ev.preventDefault();
});

// ---- 删除撤销气泡（F16 / Q10：可逆优于确认弹窗，3s 内可撤） ----

let undoTimer: number | undefined;
let undoPending: (EditContext & { lineIdx: number; removed: string[] }) | null = null;

function hideUndo(): void {
  window.clearTimeout(undoTimer);
  undoPending = null;
  root.querySelector<HTMLElement>("#undo")!.hidden = true;
}

/** 删除成功后展示 3s 气泡；撤销 = 把后端带回的原文原样插回。 */
function showUndo(c: EditContext, r: DeleteResult, label: string): void {
  undoPending = { ...c, baseVersion: r.base_version, lineIdx: r.line_idx, removed: r.removed };
  const bubble = root.querySelector<HTMLElement>("#undo")!;
  const txt = document.createElement("span");
  txt.className = "undo-txt";
  txt.textContent = `已删除「${label.length > 14 ? label.slice(0, 14) + "…" : label}」`;
  const btn = document.createElement("button");
  btn.className = "undo-btn";
  btn.textContent = "撤销";
  btn.dataset.action = "undo";
  bubble.replaceChildren(txt, btn);
  bubble.hidden = false;
  window.clearTimeout(undoTimer);
  undoTimer = window.setTimeout(hideUndo, 3000);
}

// ---- 右键菜单：v0.8 只剩「删除这条任务」（状态走左侧状态钮，受阻已随 Win UI 删除） ----

let ctxTarget: { ctx: EditContext; label: string } | null = null;

function ctxMenu(): HTMLElement {
  return root.querySelector<HTMLElement>("#ctx")!;
}

function closeCtx(): void {
  ctxTarget = null;
  ctxMenu().hidden = true;
}

/** 打开单项删除菜单；菜单按钮 data-action="ctx-delete" 回到全局委托。 */
function openDeleteMenu(item: HTMLElement, x: number, y: number): void {
  const line = Number(item.dataset.line);
  const t = taskAt(line);
  const ctx = context(line);
  if (!t || !ctx || editor || mutationPending) return;
  ctxTarget = { ctx, label: t.display };
  const menu = ctxMenu();
  const b = document.createElement("button");
  b.className = "ctx-item";
  b.textContent = "删除这条任务";
  b.dataset.action = "ctx-delete";
  menu.replaceChildren(b);
  menu.hidden = false;
  // 光标附近展开，钳制在纸面内（贴纸窗口只有 372px 宽）
  const rect = root.getBoundingClientRect();
  const mx = Math.max(0, Math.min(x - rect.left, root.clientWidth - menu.offsetWidth - 6));
  const my = Math.max(0, Math.min(y - rect.top, root.clientHeight - menu.offsetHeight - 6));
  menu.style.left = `${mx}px`;
  menu.style.top = `${my}px`;
  b.focus();
}

/** v0.8 多行拆行规则：首行=content，其余=子行；丢弃末尾连续空白子行。 */
function splitDraft(value: string): { content: string; subs: string[] } {
  const lines = value.split("\n");
  const subs = lines.slice(1);
  while (subs.length > 0 && subs[subs.length - 1].trim() === "") subs.pop();
  return { content: lines[0].trim(), subs };
}

/** 编辑文本：.txt → textarea（Shift+Enter 换行），Enter/blur 提交，Esc 取消。
 * 编辑期间隐藏旧子行容器，textarea 里以首行+子行回填，保存时整体回写。 */
function beginEdit(line: number, txt: HTMLElement): void {
  const task = taskAt(line);
  const c = context(line);
  if (!task || task.checked || !c || editor) return;
  const original = [task.content, ...task.sub_lines].join("\n");
  const input = document.createElement("textarea");
  input.className = "txt-input";
  input.rows = 1;
  input.setAttribute("aria-label", "编辑事项，Enter 保存，Shift+Enter 换行，Esc 取消");
  input.value = original;
  const autoGrow = () => { input.style.height = "auto"; input.style.height = `${input.scrollHeight}px`; };
  const subsBox = txt.closest(".body")?.querySelector<HTMLElement>(".subs");
  if (subsBox) subsBox.hidden = true;
  let pending = false;
  const cancel = () => {
    if (pending) return;
    finishEditor();
    rerender();
  };
  editor = { input, cancel };
  txt.replaceWith(input);
  autoGrow();
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
  const commit = async () => {
    if (pending || editor?.input !== input) return;
    const { content, subs } = splitDraft(input.value);
    if (!content || input.value === original) { cancel(); return; }
    pending = true;
    input.readOnly = true;
    const ok = await edit((ctx) => api.setContent(ctx.notebookId, ctx.kind, ctx.lineIdx, content, ctx.baseVersion, ctx.date, subs), c);
    pending = false;
    input.readOnly = false;
    if (ok) { finishEditor(); await refresh(); }
  };
  input.addEventListener("input", autoGrow);
  input.addEventListener("keydown", (ev) => {
    if (ev.isComposing || ev.keyCode === 229) return;
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      void commit();
    } else if (ev.key === "Escape") {
      ev.stopPropagation();
      cancel();
    }
  });
  input.addEventListener("blur", () => void commit());
}

/** 添加框：Enter 提交（Shift+Enter 换行落子行），Tab 循环分类，Esc 收起。 */
function beginAdd(): void {
  const c = context();
  if (!c || editor) return;
  const slot = root.querySelector<HTMLElement>("#add-slot")!;
  slot.replaceChildren();
  const wrap = document.createElement("div");
  wrap.className = "add-open";
  const input = document.createElement("textarea");
  input.className = "add-input";
  input.rows = 1;
  input.setAttribute("aria-label", TOUCH_DEVICE ? "新事项" : "新事项，Enter 保存，Shift+Enter 换行，Tab 切分类，Esc 取消");
  const placeholder = () => {
    const cat = ADD_CAT_CYCLE[app.addCatIdx];
    input.placeholder = TOUCH_DEVICE
      ? `新事项 · ${ADD_CAT_LABEL[cat ?? ""]}`
      : `新事项（Enter 落 [${ADD_CAT_LABEL[cat ?? ""]}]，Shift+Enter 换行，Tab 切分类）`;
  };
  placeholder();
  const autoGrow = () => { input.style.height = "auto"; input.style.height = `${input.scrollHeight}px`; };
  wrap.append(input);
  if (TOUCH_DEVICE) {
    const category = document.createElement("button");
    category.type = "button";
    category.className = "add-category";
    category.textContent = ADD_CAT_LABEL[ADD_CAT_CYCLE[app.addCatIdx] ?? ""];
    category.setAttribute("aria-label", "切换新事项分类");
    category.addEventListener("pointerdown", event => event.preventDefault());
    category.addEventListener("click", event => {
      event.stopPropagation();
      app.addCatIdx = (app.addCatIdx + 1) % ADD_CAT_CYCLE.length;
      category.textContent = ADD_CAT_LABEL[ADD_CAT_CYCLE[app.addCatIdx] ?? ""];
      placeholder();
      input.focus();
    });
    wrap.append(category);
  }
  slot.append(wrap);
  let pending = false;
  const close = () => {
    if (pending) return;
    finishEditor();
    renderAdd(root, uiState());
  };
  editor = { input, cancel: close };
  autoGrow();
  input.focus();
  const submit = async () => {
    if (pending || editor?.input !== input) return;
    const { content: text, subs } = splitDraft(input.value);
    if (!text) { close(); return; }
    pending = true;
    input.readOnly = true;
    const cat = ADD_CAT_CYCLE[app.addCatIdx];
    const ok = await edit((ctx) => api.addTask(ctx.notebookId, ctx.kind, cat, null, text, ctx.baseVersion, ctx.date, subs), c);
    pending = false;
    input.readOnly = false;
    if (ok) {
      finishEditor();
      await refresh();
      if (isCurrent(c)) beginAdd();
    } else {
      // 添加没有行号依赖，可取得新版本供用户明确按 Enter 重试。
      try {
        const latest = await api.getView(c.notebookId, c.kind, c.date);
        c.baseVersion = latest.base_version;
      } catch { /* 原始失败已显示，草稿仍在。 */ }
      input.focus();
    }
  };
  input.addEventListener("input", autoGrow);
  input.addEventListener("keydown", (ev) => {
    if (ev.isComposing || ev.keyCode === 229) return;
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      void submit();
    } else if (ev.key === "Tab") {
      ev.preventDefault();
      app.addCatIdx = (app.addCatIdx + 1) % ADD_CAT_CYCLE.length;
      placeholder();
    } else if (ev.key === "Escape") {
      ev.stopPropagation();
      close();
    }
  });
  input.addEventListener("blur", () => void submit());
}

const PRIO_CYCLE = ["P0", "P1", "P2", "P3"];

// 连点换档期间显示序冻结（render.ts frozenOrder）。1.5 秒：盖得住连点节奏
// （点击间隔通常 <1s），停手后近立即重排（初版 5s，用户反馈偏慢改短）。
const PRIO_RESORT_DELAY_MS = 1500;
let sortHoldTimer: number | undefined;

/** 优先级调整前调用：锁定当前显示序并（重）启动解冻计时，到点解锁重排一次。 */
function holdSortResort(): void {
  const tasks = app.view?.tasks ?? [];
  const keys: string[] = [];
  for (const item of root.querySelectorAll<HTMLElement>(".item")) {
    const t = tasks[Number(item.dataset.line)];
    if (t) keys.push(taskKey(t));
  }
  holdSortOrder(keys);
  if (sortHoldTimer !== undefined) window.clearTimeout(sortHoldTimer);
  sortHoldTimer = window.setTimeout(() => {
    sortHoldTimer = undefined;
    releaseSortOrder();
    rerender();
  }, PRIO_RESORT_DELAY_MS);
}

/** 删除任务并尽力弹撤销气泡（触摸行内 ✕ 与右键菜单共用）。 */
function deleteTaskFlow(captured: EditContext, label: string): void {
  void (async () => {
    let deleted: DeleteResult | undefined;
    const ok = await edit(async (c) => {
      deleted = await api.deleteTask(c.notebookId, c.kind, c.lineIdx, c.baseVersion, c.date);
      return deleted;
    }, captured);
    if (ok && deleted && isCurrent(captured) && sourceVersion(captured) === deleted.base_version)
      showUndo(captured, deleted, label);
  })();
}

/** 全局事件委托：所有交互走 data-action。 */
root.addEventListener("click", (ev) => {
  const target = ev.target as HTMLElement;
  const action = target.closest<HTMLElement>("[data-action]")?.dataset.action;
  if (!action) return;
  if (editor && action !== "sync" && action !== "add" && !target.closest("#ctx") && !canNavigate()) return;
  noteActivity();

  const item = target.closest<HTMLElement>(".item");
  const line = item ? Number(item.dataset.line) : -1;

  switch (action) {
    case "switch-notebook": {
      const notebookId = target.closest<HTMLElement>("[data-notebook-id]")?.dataset.notebookId;
      if (notebookId) switchNotebook(notebookId);
      break;
    }
    case "retry-load":
      if (TOUCH_DEVICE) void refresh();
      break;
    case "check": {
      // 行尾完成勾选框：勾=done 结算冻结；取消=复活回 paused 冻结不清零
      // （v0.8.1：复活停暂停而非进行中，点状态钮才开始续计）。
      const t = taskAt(line);
      if (!t) break;
      const target = t.checked ? "paused" : "done";
      void (async () => {
        const ok = await edit((c) => api.setStatus(c.notebookId, c.kind, c.lineIdx, target, c.baseVersion, c.date), context(line));
        if (ok) focusTaskControl(t, ".cbx"); // 完成沉底重排后焦点跟回同一任务
      })();
      break;
    }
    case "status": {
      // 左侧状态钮：单击只在 待开始→进行中 / 进行中⇄暂停 间循环；完成态零 IPC。
      const t = taskAt(line);
      if (!t || t.status === "done") break;
      const target = t.status === "doing" ? "paused" : "doing";
      void (async () => {
        const ok = await edit((c) => api.setStatus(c.notebookId, c.kind, c.lineIdx, target, c.baseVersion, c.date), context(line));
        if (ok) focusTaskControl(t, ".st");
      })();
      break;
    }
    case "prio": {
      const t = taskAt(line);
      if (!t) break;
      const cur = t.priority ? PRIO_CYCLE.indexOf(t.priority) : -1;
      const next = PRIO_CYCLE[(cur + 1) % PRIO_CYCLE.length];
      holdSortResort(); // 先锁当前显示序再换档：连点期间行不跳位，停 1.5 秒后重排
      void edit((c) => api.setPriority(c.notebookId, c.kind, c.lineIdx, next, c.baseVersion, c.date), context(line));
      break;
    }
    case "del": {
      const t = taskAt(line);
      const captured = context(line);
      if (!t || !captured) break;
      deleteTaskFlow(captured, t.display);
      break;
    }
    case "ctx-delete": {
      const pending2 = ctxTarget;
      closeCtx();
      if (pending2) deleteTaskFlow(pending2.ctx, pending2.label);
      break;
    }
    case "undo": {
      const p = undoPending;
      hideUndo();
      if (p && isCurrent(p)) void edit((c) => api.restoreDeleted(c.notebookId, c.kind, p.lineIdx, p.removed, c.baseVersion, c.date), p);
      break;
    }
    case "edit": {
      beginEdit(line, target.closest<HTMLElement>(".txt")!);
      break;
    }
    case "fold": {
      const cat = target.closest<HTMLElement>(".group")?.dataset.cat;
      if (cat) {
        if (app.fold.folded.has(cat as never)) app.fold.folded.delete(cat as never);
        else app.fold.folded.add(cat as never);
        rerender();
      }
      break;
    }
    case "carry-toggle":
      app.fold.overdueOpen = !app.fold.overdueOpen;
      rerender();
      break;
    case "carry-copy": {
      const loLine = Number(target.dataset.line);
      if (app.kind === "day" && !app.dayDate)
        void edit((c) => api.copyLeftover(c.notebookId, loLine, c.baseVersion));
      break;
    }
    case "add":
      beginAdd();
      break;
    case "day-prev":
      shiftDay(-1);
      break;
    case "day-next":
      shiftDay(1);
      break;
    case "day-today":
      if (!canNavigate()) break;
      app.dayDate = null;
      navigate();
      break;
    case "pick-date": {
      // 点报头日期 → 原生日期选择器跳选任意日期（仅日视图）
      if (app.kind !== "day" || !canNavigate()) break;
      const pick = root.querySelector<HTMLInputElement>("#date-pick")!;
      pick.value = app.dayDate ?? todayStr();
      pick.hidden = false;
      pick.focus();
      try {
        pick.showPicker();
      } catch {
        // WebView2 不支持 showPicker 时保留输入框手动键入
      }
      break;
    }
    case "sync":
      if (TOUCH_DEVICE) void touchSync();
      else void api.syncNow().catch((e) => console.error("手动同步失败", e));
      break;
  }
});

// 日期选择器：change 跳日（选中今天归 null 跟随今天），blur/Esc 收起
const datePick = root.querySelector<HTMLInputElement>("#date-pick")!;
datePick.addEventListener("change", () => {
  const v = datePick.value;
  datePick.hidden = true;
  if (!v) return;
  if (!canNavigate()) return;
  app.dayDate = v === todayStr() ? null : v;
  navigate();
});
datePick.addEventListener("blur", () => {
  datePick.hidden = true;
});
datePick.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") {
    ev.stopPropagation();
    datePick.hidden = true;
  }
});

/** 翻日：目标日期 ±N 天；目标恰为今天时 dayDate 归 null（跟随今天）。
 * 中午 12 点锚定避开日界。 */
function shiftDay(days: number): void {
  if (!canNavigate()) return;
  const cur = app.dayDate ?? todayStr();
  const nd = new Date(cur + "T12:00:00");
  nd.setDate(nd.getDate() + days);
  const next = fmtDate(nd);
  app.dayDate = next === todayStr() ? null : next;
  navigate();
}

root.querySelector(".tabs")!.addEventListener("click", (ev) => {
  const btn = (ev.target as HTMLElement).closest<HTMLElement>(".tab");
  if (!btn) return;
  const kind = btn.dataset.kind as ViewKind;
  if (kind === app.kind) return;
  if (!canNavigate()) return;
  app.kind = kind;
  app.fold.folded.clear();
  navigate();
});

/** 右键任务行 → 单项删除菜单（完成项也可删；编辑/写入中不弹）。 */
root.addEventListener("contextmenu", (ev) => {
  const item = (ev.target as HTMLElement).closest<HTMLElement>(".item");
  if (!item) return;
  ev.preventDefault();
  openDeleteMenu(item, ev.clientX, ev.clientY);
});

// 键盘等效：Menu / Shift+F10 对焦点所在任务行开同一菜单
window.addEventListener("keydown", (ev) => {
  if (ev.isComposing || ev.keyCode === 229) return;
  if (ev.key !== "ContextMenu" && !(ev.key === "F10" && ev.shiftKey)) return;
  const item = (document.activeElement as HTMLElement | null)?.closest<HTMLElement>(".item");
  if (!item) return;
  ev.preventDefault();
  const r = item.getBoundingClientRect();
  openDeleteMenu(item, r.left, r.bottom);
});

// 菜单外点击关闭（pointerdown 先于 click 到达；点菜单项本身不在此关）
window.addEventListener("pointerdown", (ev) => {
  if (ctxMenu().hidden) return;
  if (!(ev.target as HTMLElement).closest("#ctx")) closeCtx();
}, true);

// ---- 沉回 / 焦点（设计方案 §0：唤起 5s 无操作或 Esc 沉回桌面层） ----

let sinkTimer: number | undefined;

function sink(): void {
  if (TOUCH_DEVICE) return;
  void api.shellSink().catch(() => {});
}

function armSink(): void {
  if (TOUCH_DEVICE) return;
  window.clearTimeout(sinkTimer);
  sinkTimer = window.setTimeout(sink, SINK_IDLE_MS);
}

function noteActivity(): void {
  armSink();
}

for (const evName of TOUCH_DEVICE ? [] : ["pointerdown", "keydown", "pointermove"] as const) {
  window.addEventListener(evName, noteActivity, { passive: true });
}
window.addEventListener("keydown", (ev) => {
  if (ev.isComposing || ev.keyCode === 229) return;
  if (ev.key !== "Escape") return;
  if (!notesPanel.hidden) {
    ev.preventDefault();
    closeNotes();
    return;
  }
  if (!notebookPanel.hidden) {
    ev.preventDefault();
    closeNotebooks();
    return;
  }
  if (editor) { editor.cancel(); return; }
  if (!ctxMenu().hidden) {
    closeCtx(); // Esc 分层：先关菜单，不沉回
    return;
  }
  sink();
});
armSink();

// ---- 高度自适应（渲染后显式量高 + ResizeObserver 兜底，100ms 节流） ----

let heightTimer: number | undefined;
let lastHeight = -1;

function measureHeight(): void {
  if (TOUCH_DEVICE) return;
  window.clearTimeout(heightTimer);
  heightTimer = window.setTimeout(() => {
    // 量 #sticky 内容高：documentElement/body.scrollHeight 有 ≥客户区的
    // 隐性地板（scrollHeight ≥ clientHeight），窗口只会长高永不回缩；
    // 窗口改不透明后内容短时会露出空白面板尾，必须按内容真缩。
    const h = document.getElementById("sticky")?.scrollHeight ?? 0;
    if (h > 0 && Math.abs(h - lastHeight) > 1) {
      lastHeight = h;
      // +2px 余量：分数 DPI 下 LogicalSize→物理像素舍入会让窗口矮亚像素，
      // 触发窗口级滚动条闪现 → 8px 宽度翻转 → 行换行翻转 → 高度翻转的
      // 持续抖动（2026-09-14：单行日切周复现）。余量让溢出永不成立。
      void api.setBodyHeight(h + 2).catch(() => {});
    }
  }, 100);
}

const ro = new ResizeObserver(measureHeight);
if (!TOUCH_DEVICE) ro.observe(document.getElementById("sticky")!);

// ---- 后端事件 ----

let touchSyncPending = false;
let touchSyncKind: SyncState["kind"] = "idle";
let pullHint: HTMLElement | null = null;
function showPull(text = ""): void {
  if (!pullHint) return;
  pullHint.textContent = text;
  pullHint.hidden = !text;
}
function updateSync(status: SyncState): void {
  renderSync(root, status);
  if (!TOUCH_DEVICE) return;
  touchSyncKind = status.kind;
  if (status.kind !== "syncing") touchSyncPending = false;
  showPull(status.kind === "syncing" ? "同步中…" : "");
}
async function touchSync(): Promise<void> {
  if (touchSyncPending || touchSyncKind === "syncing" || syncSettingsBusy) return;
  touchSyncPending = true;
  showPull("同步中…");
  try {
    // syncNow only queues work; sync-status owns completion and releases the guard.
    await api.syncNow();
  } catch (error) {
    touchSyncPending = false;
    updateSync({ kind: "error", message: error instanceof Error ? error.message : String(error) });
  }
}

if (TOUCH_DEVICE) {
  // Observe the existing close paths (including Esc, blur and outside clicks).
  // Reconcile one history entry at a time: back() is asynchronous, and panels
  // can reopen or replace one another before its popstate arrives.
  const panels = [
    { node: notesPanel, open: () => !notesPanel.hidden, close: () => closeNotes() },
    { node: notebookPanel, open: () => !notebookPanel.hidden, close: () => closeNotebooks() },
    { node: notebookForm, open: () => !notebookPanel.hidden && !notebookForm.hidden,
      close: () => { if (!notebookSaving) notebookForm.hidden = true; } },
    { node: syncSettingsForm, open: () => !notebookPanel.hidden && !syncSettingsForm.hidden,
      close: () => { syncSettingsForm.hidden = true; syncPat.value = ""; syncSettingsToggle.setAttribute("aria-expanded", "false"); } },
    { node: appearance, open: () => appearance.open, close: () => { appearance.open = false; } },
    { node: datePick, open: () => !datePick.hidden, close: () => { datePick.hidden = true; datePick.blur(); } },
    { node: ctxMenu(), open: () => !ctxMenu().hidden, close: () => closeCtx() },
    // 任意行内编辑器（任务 textarea / 添加框）都算这个面板打开，返回键统一走 cancel。
    { node: root.querySelector<HTMLElement>("#add-slot")!, open: () => !!editor, close: () => editor?.cancel() },
  ];
  type Panel = typeof panels[number];
  let active: Panel[] = [];
  const sentinels: Panel[] = [];
  let cleaning = false;
  const reconcile = () => {
    active = active.filter(panel => panel.open());
    for (const panel of panels) if (panel.open() && !active.includes(panel)) active.push(panel);
    if (cleaning) return;
    const common = sentinels.findIndex((panel, index) => active[index] !== panel);
    if (common !== -1) {
      cleaning = true;
      history.back();
      return;
    }
    for (const panel of active.slice(sentinels.length)) {
      history.pushState({ stickyPanel: panel.node.id }, "");
      sentinels.push(panel);
    }
  };
  const observer = new MutationObserver(reconcile);
  for (const panel of panels) observer.observe(panel.node, { attributes: true, attributeFilter: ["hidden", "open"] });
  observer.observe(root, { childList: true, subtree: true });
  window.addEventListener("popstate", () => {
    const panel = sentinels.pop();
    if (!cleaning) panel?.close();
    cleaning = false;
    reconcile();
  });
  window.addEventListener("keydown", event => {
    if (event.key !== "Escape" || event.isComposing || event.keyCode === 229) return;
    reconcile();
    const panel = active[active.length - 1];
    if (!panel) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    panel.close();
    reconcile();
  }, true);
  reconcile();

  pullHint = document.createElement("div");
  pullHint.className = "pull-hint";
  pullHint.setAttribute("role", "status");
  pullHint.hidden = true;
  root.insertBefore(pullHint, root.querySelector("#groups"));
  // The list scrolls with the document; footer panels have independent scrolling.
  const scroller = document.scrollingElement ?? document.documentElement;
  let gesture: { x: number; y: number; dx: number; dy: number; maxY: number;
    axis: "x" | "y" | null; top: boolean; moved: boolean } | null = null;
  let suppressClickUntil = 0;
  const busy = () => touchSyncPending || touchSyncKind === "syncing";
  const cancelGesture = () => {
    if (gesture?.moved) suppressClickUntil = performance.now() + 500;
    gesture = null;
    if (!busy()) showPull();
  };
  root.addEventListener("touchstart", event => {
    suppressClickUntil = 0;
    const target = event.target as HTMLElement;
    if (event.touches.length !== 1 || editor || mutationPending || panels.some(panel => panel.open()) ||
      target.closest(".footer-area, .mast, .tabs, #add-slot, #undo, input, textarea, select, [contenteditable]")) {
      cancelGesture(); return;
    }
    const touch = event.touches[0];
    gesture = { x: touch.clientX, y: touch.clientY, dx: 0, dy: 0, maxY: 0,
      axis: null, top: scroller.scrollTop <= 0, moved: false };
  }, { passive: true });
  root.addEventListener("touchmove", event => {
    if (!gesture) return;
    if (event.touches.length !== 1 || panels.some(panel => panel.open())) { cancelGesture(); return; }
    const touch = event.touches[0];
    gesture.dx = touch.clientX - gesture.x;
    gesture.dy = touch.clientY - gesture.y;
    const x = Math.abs(gesture.dx), y = Math.abs(gesture.dy);
    gesture.maxY = Math.max(gesture.maxY, y);
    if (!gesture.axis && Math.max(x, y) > 8) {
      gesture.axis = x > y ? "x" : "y";
      gesture.moved = true;
    }
    const pulling = gesture.axis === "y" && gesture.top && gesture.dy > 0 && scroller.scrollTop <= 0;
    if (gesture.axis === "x" || pulling) {
      if (event.cancelable) event.preventDefault();
      else { cancelGesture(); return; }
    }
    if (pulling) showPull(busy() ? "同步中…" : gesture.dy > 60 ? "松开同步…" : "下拉同步…");
    else if (!busy()) showPull();
  }, { passive: false });
  root.addEventListener("touchend", event => {
    const completed = gesture;
    cancelGesture();
    if (!completed || event.touches.length || editor || mutationPending || panels.some(panel => panel.open())) return;
    if (completed.axis === "x" && Math.abs(completed.dx) > 80 && completed.maxY < 40) {
      shiftDay((completed.dx > 0 ? -1 : 1) * (app.kind === "week" ? 7 : 1));
    } else if (completed.axis === "y" && completed.top && completed.dy > 60 && scroller.scrollTop <= 0) {
      void touchSync();
    }
  }, { passive: true });
  root.addEventListener("touchcancel", cancelGesture, { passive: true });
  root.addEventListener("click", event => {
    if (performance.now() < suppressClickUntil) { event.preventDefault(); event.stopImmediatePropagation(); }
  }, true);
  root.addEventListener("contextmenu", event => {
    const moved = gesture?.moved || performance.now() < suppressClickUntil;
    cancelGesture();
    if (moved) { event.preventDefault(); event.stopImmediatePropagation(); }
  }, true);
}

void listen("repo-ready", () => {
  app.repoReady = true;
  void refresh();
});
void listen("view-changed", () => {
  if (app.repoReady) void refresh();
  if (!notesPanel.hidden && !dirtyNotes().length) void loadNotes();
});

// 窗口右上角关闭（仅桌面；安卓 WebView UA 含 Android，保持 hidden）。
// 点击后先把速记草稿落盘、等在途编辑收尾（有界 3s），再走窗口关闭 →
// ExitRequested 既有 15s flush，不新增退出通道。内联编辑器草稿不阻塞关闭。
const winCloseBtn = root.querySelector<HTMLButtonElement>("#win-close");
if (winCloseBtn && !/android/i.test(navigator.userAgent)) {
  winCloseBtn.hidden = false;
  winCloseBtn.addEventListener("click", () => { void closeWindow(); });
}

async function closeWindow(): Promise<void> {
  if (!TOUCH_DEVICE) {
    const saved = await saveNote(noteSquare).catch(() => false);
    if (!saved) {
      cacheNoteSquare(noteSquare.dataset.noteDate ?? todayStr(), noteSquare.value,
        noteSquare.dataset.notebookId ?? app.notebookId);
    }
    // 速记面板里未保存的草稿同样落盘，不再只依赖 ExitRequested 兜底。
    await Promise.all(Array.from(notesBody.querySelectorAll<HTMLTextAreaElement>(".notes-input"))
      .filter(input => input.value !== input.dataset.loaded)
      .map(input => saveNote(input).catch(() => false)));
  }
  const deadline = Date.now() + 3000;
  while (Date.now() < deadline && (mutationPending || noteSquare.dataset.saving === "true" || notesBody.querySelector('[data-saving="true"]'))) {
    await new Promise(resolve => setTimeout(resolve, 80));
  }
  try {
    await getCurrentWindow().close();
  } catch {
    winCloseBtn?.blur(); // 关闭被拒（如权限缺失）时至少移走焦点
  }
}

// 先完成监听再读快照；请求途中收到的新事件优先于旧快照。
void (async () => {
  let statusRevision = 0;
  await listen<SyncState>("sync-status", (ev) => {
    statusRevision += 1;
    updateSync(ev.payload);
  });
  const requestedRevision = statusRevision;
  const status = await api.syncStatus();
  if (statusRevision === requestedRevision) updateSync(status);
})().catch(() => updateSync({
  kind: "error",
  message: "无法读取或监听同步状态，请重启工具后重试",
}));
// 本机提交身份保留在同步提示中，页脚留给常用入口。
void api
  .machineTag()
  .then((t) => {
    root.querySelector<HTMLElement>("#sync-tag")!.textContent = `sticky@${t}`;
    root.querySelector<HTMLElement>(".foot")!.title = `点击立即同步 · sticky@${t}`;
  })
  .catch(() => {});
void refresh(); // 仓库已就绪时立即出数据（repo-ready 可能已错过）
rerender();
