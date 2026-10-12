/** Windows 更新面板自足模块：只消费后端状态，不让 WebView 直接访问 GitHub。 */
import { listen } from "@tauri-apps/api/event";
import { api, type UpdateStatus } from "./api";

const version = document.querySelector<HTMLElement>("#app-version")!;
const status = document.querySelector<HTMLElement>("#update-status")!;
const check = document.querySelector<HTMLButtonElement>("#update-check")!;
const pat = document.querySelector<HTMLInputElement>("#update-pat")!;
const patLabel = document.querySelector<HTMLLabelElement>('label[for="update-pat"]')!;

const labels: Record<UpdateStatus["state"], string> = {
  idle: "已是最新版",
  checking: "正在检查…",
  downloading: "正在下载…",
  ready: "已就绪，稍后重启",
  updated: "正在重启…",
  error: "更新失败",
  no_token: "需要 GitHub 令牌",
};

function render(value: UpdateStatus): void {
  version.textContent = value.current;
  status.textContent = value.message || labels[value.state];
  status.title = value.latest ? `最新版本 ${value.latest}` : "";
  const needsToken = value.state === "no_token";
  pat.hidden = !needsToken;
  patLabel.hidden = !needsToken;
  check.hidden = false;
  check.disabled = value.state === "checking" || value.state === "downloading" || value.state === "updated";
}

check.addEventListener("click", async () => {
  check.disabled = true;
  try {
    // 空输入表示保留原值；密码框永不从配置回填。
    const updatePat = pat.value.trim();
    pat.value = "";
    if (updatePat) await api.setSyncConfig(undefined, undefined, updatePat);
    await api.updateCheckNow();
  } catch (error) {
    status.textContent = error instanceof Error ? error.message : "无法检查更新";
    check.disabled = false;
  }
});

void listen<UpdateStatus>("update-status", event => render(event.payload));
void api.updateStatus().then(render).catch(() => {
  status.textContent = "状态不可用";
  check.hidden = false;
});
