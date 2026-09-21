/** 后端契约的 TypeScript 镜像（store.rs DTO，serde 默认：枚举大驼峰、字段 snake_case）。 */

export type Priority = "P0" | "P1" | "P2" | "P3";
export type Category = "Work" | "Personal" | "Uncategorized";
export type ViewKind = "day" | "week";
/** serde(rename_all = "lowercase") 的 TaskStatus：待开始一次性初始态，永不回归 */
export type TaskStatus = "todo" | "doing" | "paused" | "done";

export interface NoteDayDto { date: string; weekday: string; content: string; base_version: string }
export interface NotesDto { kind: ViewKind; date: string; days: NoteDayDto[] }

export interface Notebook { id: string; name: string; base_version: string }
export interface TaskSource { kind: ViewKind; date: string; rel_path: string; base_version: string }

export interface TaskView {
  row_idx: number;
  source: TaskSource;
  line_idx: number;
  checked: boolean;
  priority: Priority | null;
  category: Category;
  /** 展示文本（内容 + 人工注记），不含状态标签 */
  display: string;
  /** 纯内容段（编辑时替换的部分） */
  content: string;
  doing: boolean;
  overdue: boolean;
  blocked_reason: string | null;
  /** v0.8 四态派生（checked > #paused > #doing > 仅 t/ts > todo） */
  status: TaskStatus;
  /** 已结算累计秒（含 Doing 态的进行中增量由前端 ticker 现算） */
  timer_secs: number;
  /** Doing 态的起点 Unix 秒；非 Doing 或无 #ts 时为 null */
  timer_started_at: number | null;
  /** 行内存在非法 #t token（值取最后合法值，标签由下次状态手术清理） */
  timer_invalid: boolean;
  sub_lines: string[];
  section: string;
  subsection: string | null;
}

export interface ViewDto {
  notebook_id: string;
  sources: TaskSource[];
  kind: ViewKind;
  date: string;
  rel_path: string;
  exists: boolean;
  /** u64 内容哈希跨界为字符串（>2^53 的 Number 经 JSON 往返丢精度会永远 stale） */
  base_version: string;
  tasks: TaskView[];
}

export interface LeftoverDto {
  line_idx: number;
  content: string;
  priority: Priority | null;
  category: Category;
}

export interface LeftoversDto {
  date: string;
  count: number;
  tasks: LeftoverDto[];
}

/** 删除结果：带回被删原文与行号，撤销气泡用其精确恢复。 */
export interface DeleteResult {
  base_version: string;
  line_idx: number;
  removed: string[];
}

export type SyncState =
  | { kind: "idle" }
  | { kind: "syncing" }
  | { kind: "offline"; unpushed: number }
  | { kind: "conflict"; message: string }
  | { kind: "error"; message: string };

export class ApiError extends Error {
  constructor(
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

import { invoke } from "@tauri-apps/api/core";

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    // Rust 侧 CmdError → { code, message }
    if (e && typeof e === "object" && "code" in e) {
      const err = e as { code: string; message?: string };
      throw new ApiError(err.code, err.message ?? "未知错误");
    }
    throw new ApiError("internal", String(e));
  }
}

export const api = {
  getNotes: (notebookId: string, kind: ViewKind, date?: string | null) =>
    call<NotesDto>("get_notes", { notebookId, kind, date }),
  saveNote: (notebookId: string, date: string, content: string, baseVersion: string) =>
    call<{ base_version: string }>("save_note", { notebookId, date, content, baseVersion }),
  listNotebooks: () => call<Notebook[]>("list_notebooks"),
  createNotebook: (name: string) => call<Notebook>("create_notebook", { name }),
  renameNotebook: (notebookId: string, name: string, baseVersion: string) =>
    call<Notebook>("rename_notebook", { notebookId, name, baseVersion }),
  getView: (notebookId: string, kind: ViewKind, date?: string | null) =>
    call<ViewDto>("get_view", { notebookId, kind, date }).then(view => ({
      ...view, tasks: view.tasks.map((task, row_idx) => ({ ...task, row_idx })),
    })),
  getLeftovers: (notebookId: string) => call<LeftoversDto>("get_yesterday_leftovers", { notebookId }),
  copyLeftover: (notebookId: string, lineIdx: number, baseVersion: string) =>
    call<{ base_version: string; line_idx: number }>("copy_leftover_to_today", {
      notebookId,
      lineIdx,
      baseVersion,
    }),
  setPriority: (
    notebookId: string,
    kind: ViewKind,
    lineIdx: number,
    priority: string | null,
    baseVersion: string,
    date?: string | null,
  ) => call<{ base_version: string }>("set_priority", { notebookId, kind, lineIdx, priority, baseVersion, date }),
  setContent: (
    notebookId: string,
    kind: ViewKind,
    lineIdx: number,
    content: string,
    baseVersion: string,
    date?: string | null,
    /** v0.8 多行：不传=保留旧子行 / []=清空 / 非空=整体替换（undefined 键不序列化 → None） */
    subLines?: string[],
  ) =>
    call<{ base_version: string }>("set_content", { notebookId, kind, lineIdx, content, baseVersion, date, subLines }),
  setStatus: (
    notebookId: string,
    kind: ViewKind,
    lineIdx: number,
    /** "todo" 后端拒绝（待开始不可回归）；done=完成冻结、doing=复活续计 */
    target: "doing" | "paused" | "done",
    baseVersion: string,
    date?: string | null,
  ) => call<{ base_version: string }>("set_status", { notebookId, kind, lineIdx, target, baseVersion, date }),
  deleteTask: (
    notebookId: string,
    kind: ViewKind,
    lineIdx: number,
    baseVersion: string,
    date?: string | null,
  ) => call<DeleteResult>("delete_task", { notebookId, kind, lineIdx, baseVersion, date }),
  restoreDeleted: (
    notebookId: string,
    kind: ViewKind,
    lineIdx: number,
    removed: string[],
    baseVersion: string,
    date?: string | null,
  ) => call<{ base_version: string }>("restore_deleted", { notebookId, kind, lineIdx, removed, baseVersion, date }),
  addTask: (
    notebookId: string,
    kind: ViewKind,
    category: string | null,
    priority: string | null,
    text: string,
    baseVersion: string,
    date?: string | null,
    /** v0.8 多行录入：不传/空数组=无子行 */
    subLines?: string[],
  ) =>
    call<{ base_version: string; line_idx: number }>("add_task", {
      notebookId,
      kind,
      category,
      priority,
      text,
      baseVersion,
      date,
      subLines,
    }),
  syncNow: () => call<null>("sync_now"),
  getSyncConfig: () => call<{ repo_url: string; pat_set: boolean }>("get_sync_config"),
  setSyncConfig: (repoUrl?: string, pat?: string) =>
    call<void>("set_sync_config", { repoUrl, pat }),
  syncStatus: () => call<SyncState>("sync_status"),
  machineTag: () => call<string>("machine_tag"),
  shellSink: () => call<null>("shell_sink"),
  setBodyHeight: (height: number) => call<null>("set_body_height", { height }),
};
