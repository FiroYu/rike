/** 本机外观偏好独立于任务数据，切换不会重绘清单或打断输入。 */
export {};
// 毛玻璃依赖 Windows 系统亚克力（tauri.conf windowEffects + transparent），
// 其他端（Android WebView）不投放该选项，外观与默认值均回到 classic。
const isWindows = /Windows NT/.test(navigator.userAgent);
const themes = ([
  { id: "glass", name: "毛玻璃", paper: "#e9edf4", ink: "#16181a", detail: "玻璃 · 透桌面" },
  { id: "classic", name: "极简黑标", paper: "#f4f3f0", ink: "#16181a", detail: "素纸 · 黑墨" },
  { id: "midnight", name: "午夜墨蓝", paper: "#1d2837", ink: "#edf1f7", detail: "深色纸 · 银墨" },
  { id: "mist", name: "雾蓝点阵", paper: "#edf3f8", ink: "#355571", detail: "点阵纸 · 雾蓝" },
] as const).filter((t) => isWindows || t.id !== "glass");

const storageKey = "sticky-todo.theme";
const options = document.querySelector<HTMLElement>("#theme-options")!;
const panel = document.querySelector<HTMLDetailsElement>("#appearance")!;
const themeStatus = document.querySelector<HTMLElement>("#theme-status")!;

// v0.8 字号五档：正文 px 乘数挂 --fs-scale，存档位索引（与主题同键位语义：本机私有）。
const FONT_TIERS = [12, 13, 14, 15, 16] as const;
const FONT_DEFAULT_IDX = 1;
const fontStorageKey = "sticky.fontscale";
const fontDown = document.querySelector<HTMLButtonElement>("#font-down")!;
const fontUp = document.querySelector<HTMLButtonElement>("#font-up")!;
const fontLabel = document.querySelector<HTMLElement>("#font-size-label")!;
let fontIdx = FONT_DEFAULT_IDX;

function applyFontSize(idx: number, persist: boolean): void {
  fontIdx = Math.min(Math.max(Math.trunc(idx), 0), FONT_TIERS.length - 1);
  const px = FONT_TIERS[fontIdx];
  document.documentElement.style.setProperty("--fs-scale", String(px / 13));
  fontLabel.textContent = String(px);
  fontDown.disabled = fontIdx === 0;
  fontUp.disabled = fontIdx === FONT_TIERS.length - 1;
  if (persist) {
    try {
      localStorage.setItem(fontStorageKey, String(fontIdx));
      themeStatus.textContent = "已保存 · 仅用于这台设备";
    } catch {
      themeStatus.textContent = "已调整；当前无法保存，重启后将恢复默认";
    }
  }
}

fontDown.addEventListener("click", () => applyFontSize(fontIdx - 1, true));
fontUp.addEventListener("click", () => applyFontSize(fontIdx + 1, true));

function applyTheme(id: string, persist: boolean): void {
  const theme = themes.find((t) => t.id === id) ?? themes[0];
  document.documentElement.dataset.theme = theme.id;
  document.querySelector<HTMLElement>("#theme-name")!.textContent = theme.name;
  options.querySelectorAll<HTMLButtonElement>("button").forEach((button) => {
    button.setAttribute("aria-pressed", String(button.dataset.theme === theme.id));
  });
  if (persist) {
    try {
      localStorage.setItem(storageKey, theme.id);
      themeStatus.textContent = "已保存 · 仅用于这台设备";
    } catch {
      themeStatus.textContent = "已切换；当前无法保存，重启后将恢复默认";
    }
  }
}

for (const theme of themes) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "theme-option";
  button.dataset.theme = theme.id;
  button.title = theme.detail;
  button.style.setProperty("--swatch-paper", theme.paper);
  button.style.setProperty("--swatch-ink", theme.ink);
  const swatch = document.createElement("span");
  swatch.className = "theme-swatch";
  swatch.setAttribute("aria-hidden", "true");
  swatch.textContent = "Aa";
  const label = document.createElement("span");
  label.textContent = theme.name;
  button.append(swatch, label);
  button.addEventListener("click", () => applyTheme(theme.id, true));
  options.append(button);
}

panel.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && panel.open) {
    event.stopPropagation();
    panel.open = false;
    panel.querySelector("summary")!.focus();
  }
});

let saved: string = themes[0].id; // 无存档时默认首位（Windows=毛玻璃，其他端=极简黑标）
try { saved = localStorage.getItem(storageKey) ?? saved; } catch { /* 当前会话仍可切换外观。 */ }
applyTheme(saved, false);

let savedFontIdx = FONT_DEFAULT_IDX;
try {
  const raw = Number.parseInt(localStorage.getItem(fontStorageKey) ?? "", 10);
  if (Number.isInteger(raw) && raw >= 0 && raw < FONT_TIERS.length) savedFontIdx = raw;
} catch { /* 读取失败用默认档，会话内仍可调节。 */ }
applyFontSize(savedFontIdx, false);
