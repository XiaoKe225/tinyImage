import { invoke } from "@tauri-apps/api/core";
import { LogicalPosition } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

type ProgressEvent = {
  done: number;
  total: number;
  success: number;
  failed: number;
  skipped: number;
  savedBytes: number;
  currentPath?: string | null;
  lastError?: string | null;
  cancelled: boolean;
};

type FailItem = { path: string; error: string };

type BatchSummary = {
  total: number;
  success: number;
  failed: number;
  skipped: number;
  savedBytes: number;
  cancelled: boolean;
  failures: FailItem[];
  skips: FailItem[];
};

/** 本机记住：力度 + 窗口位置；窗口大小不做持久化（每次用配置默认） */
const SETTINGS_KEY = "tinyimage.settings.v1";

type AppSettings = {
  intensity: number;
  x?: number;
  y?: number;
};

let busy = false;
let lastFailures: FailItem[] = [];
let lastSkips: FailItem[] = [];
let savePosTimer: ReturnType<typeof setTimeout> | null = null;

const el = {
  drop: () => document.getElementById("drop"),
  status: () => document.getElementById("status"),
  progressWrap: () => document.getElementById("progress-wrap"),
  barFill: () => document.getElementById("bar-fill"),
  progressText: () => document.getElementById("progress-text"),
  btnCancel: () => document.getElementById("btn-cancel") as HTMLButtonElement | null,
  failPanel: () => document.getElementById("fail-panel"),
  failList: () => document.getElementById("fail-list"),
  btnCopyFail: () => document.getElementById("btn-copy-fail") as HTMLButtonElement | null,
  skipPanel: () => document.getElementById("skip-panel"),
  skipList: () => document.getElementById("skip-list"),
  intensity: () => document.getElementById("intensity") as HTMLInputElement | null,
  intensityVal: () => document.getElementById("intensity-val"),
};

function clampIntensity(n: number): number {
  if (!Number.isFinite(n)) return 0;
  return Math.max(0, Math.min(100, Math.round(n)));
}

function loadSettings(): AppSettings {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (!raw) return { intensity: 0 };
    const o = JSON.parse(raw) as Partial<AppSettings> & {
      width?: number;
      height?: number;
    };
    return {
      intensity: clampIntensity(Number(o.intensity ?? 0)),
      x: typeof o.x === "number" ? o.x : undefined,
      y: typeof o.y === "number" ? o.y : undefined,
      // 故意忽略历史 width/height：窗口大小不做持久化
    };
  } catch {
    return { intensity: 0 };
  }
}

function saveSettings(partial: Partial<AppSettings>) {
  const cur = loadSettings();
  const next: AppSettings = {
    intensity: clampIntensity(
      partial.intensity !== undefined ? partial.intensity : cur.intensity,
    ),
    x: partial.x !== undefined ? partial.x : cur.x,
    y: partial.y !== undefined ? partial.y : cur.y,
  };
  try {
    // 不写 width/height，顺带清掉旧版里的尺寸字段
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(next));
  } catch {
    /* 存储满/禁用时忽略，不影响压缩 */
  }
}

function getIntensity(): number {
  const input = el.intensity();
  if (!input) return loadSettings().intensity;
  return clampIntensity(Number(input.value));
}

/** 与后端 quality_percent_from_intensity 一致 */
function qualityPercent(intensity: number): number {
  const i = clampIntensity(intensity);
  if (i === 0) return 100;
  if (i <= 34) return Math.round(96 - (i * 21) / 34);
  return Math.round(75 - ((i - 34) * 33) / 66);
}

function syncIntensityLabel() {
  const intensity = getIntensity();
  const node = el.intensityVal();
  if (node) node.textContent = String(intensity);
  const q = document.getElementById("quality-pct");
  if (q) q.textContent = String(qualityPercent(intensity));
}

function applyIntensityFromSettings() {
  const s = loadSettings();
  const input = el.intensity();
  if (input) input.value = String(s.intensity);
  syncIntensityLabel();
}

async function restoreWindowPosition() {
  const s = loadSettings();
  if (typeof s.x !== "number" || typeof s.y !== "number") return;
  try {
    await getCurrentWindow().setPosition(new LogicalPosition(s.x, s.y));
  } catch {
    /* 权限/多显示器异常时保持系统默认位置 */
  }
}

function scheduleSaveWindowPosition() {
  if (savePosTimer) clearTimeout(savePosTimer);
  savePosTimer = setTimeout(() => {
    void (async () => {
      try {
        const win = getCurrentWindow();
        const pos = await win.outerPosition();
        const factor = await win.scaleFactor();
        saveSettings({
          x: Math.round(pos.x / factor),
          y: Math.round(pos.y / factor),
        });
      } catch {
        /* ignore */
      }
    })();
  }, 280);
}

/** 自绘确认框：避免 WebView 原生 confirm 在窄窗裁切「取消」 */
function confirmReplace(message: string): Promise<boolean> {
  const modal = document.getElementById("confirm-modal");
  const msg = document.getElementById("confirm-msg");
  const okBtn = document.getElementById("confirm-ok");
  const cancelBtn = document.getElementById("confirm-cancel");
  const backdrop = document.getElementById("confirm-backdrop");
  if (!modal || !msg || !okBtn || !cancelBtn) {
    return Promise.resolve(window.confirm(message));
  }
  msg.textContent = message;
  modal.hidden = false;
  return new Promise((resolve) => {
    const finish = (v: boolean) => {
      modal.hidden = true;
      okBtn.removeEventListener("click", onOk);
      cancelBtn.removeEventListener("click", onCancel);
      backdrop?.removeEventListener("click", onCancel);
      document.removeEventListener("keydown", onKey);
      resolve(v);
    };
    const onOk = () => finish(true);
    const onCancel = () => finish(false);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") finish(false);
      if (e.key === "Enter") finish(true);
    };
    okBtn.addEventListener("click", onOk);
    cancelBtn.addEventListener("click", onCancel);
    backdrop?.addEventListener("click", onCancel);
    document.addEventListener("keydown", onKey);
    okBtn.focus();
  });
}

function setStatus(text: string, isError = false) {
  const node = el.status();
  if (!node) return;
  node.textContent = text;
  node.classList.toggle("error", isError);
}

function formatBytes(n: number): string {
  if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)}M`;
  if (n >= 1024) return `${Math.round(n / 1024)}K`;
  return `${n}B`;
}

function setBusy(v: boolean) {
  busy = v;
  el.drop()?.classList.toggle("busy", v);
  const wrap = el.progressWrap();
  if (wrap) wrap.hidden = !v;
  const btn = el.btnCancel();
  if (btn) btn.disabled = !v;
  const range = el.intensity();
  if (range) range.disabled = v;
}

function fillList(listEl: HTMLElement | null, items: FailItem[]) {
  if (!listEl) return;
  listEl.innerHTML = "";
  for (const f of items) {
    const li = document.createElement("li");
    li.textContent = `${f.path || "(未知)"} — ${f.error}`;
    listEl.appendChild(li);
  }
}

function renderFailures(failures: FailItem[]) {
  lastFailures = failures;
  const panel = el.failPanel();
  if (!panel) return;
  if (!failures.length) {
    panel.hidden = true;
    fillList(el.failList(), []);
    return;
  }
  panel.hidden = false;
  fillList(el.failList(), failures);
}

function renderSkips(skips: FailItem[]) {
  lastSkips = skips;
  const panel = el.skipPanel();
  if (!panel) return;
  if (!skips.length) {
    panel.hidden = true;
    fillList(el.skipList(), []);
    return;
  }
  panel.hidden = false;
  fillList(el.skipList(), skips);
}

function updateProgress(p: ProgressEvent) {
  const pct = p.total > 0 ? (p.done / p.total) * 100 : 0;
  const bar = el.barFill();
  if (bar) bar.style.width = `${pct}%`;
  const text = el.progressText();
  if (text) {
    text.textContent = `${p.done}/${p.total}  成功 ${p.success}  失败 ${p.failed}  跳过 ${p.skipped}`;
  }
}

async function startBatch(paths: string[]) {
  if (busy) return;

  const intensity = getIntensity();
  saveSettings({ intensity });

  let images: string[];
  try {
    images = await invoke<string[]>("collect_images", { paths });
  } catch (e) {
    setStatus(String(e), true);
    renderFailures([]);
    renderSkips([]);
    return;
  }

  const ok = await confirmReplace(
    `将压缩 ${images.length} 张图片并替换原文件。\n力度 ${intensity}（质量约 ${qualityPercent(intensity)}%）\n是否继续？`,
  );
  if (!ok) {
    setStatus("已取消");
    return;
  }

  setBusy(true);
  renderFailures([]);
  renderSkips([]);
  updateProgress({
    done: 0,
    total: images.length,
    success: 0,
    failed: 0,
    skipped: 0,
    savedBytes: 0,
    cancelled: false,
  });
  setStatus(`压缩中…（强度 ${intensity}）`);

  try {
    const summary = await invoke<BatchSummary>("compress_batch", {
      paths: images,
      intensity,
    });
    const parts = [
      summary.cancelled ? "已取消" : "完成",
      `强度 ${intensity}`,
      `成功 ${summary.success}`,
      `失败 ${summary.failed}`,
      `跳过 ${summary.skipped}`,
      `节省 ${formatBytes(summary.savedBytes)}`,
    ];
    const warn = summary.failed > 0 || (summary.success === 0 && summary.skipped > 0);
    setStatus(parts.join(" · "), warn);
    renderFailures(summary.failures ?? []);
    renderSkips(summary.skips ?? []);
  } catch (e) {
    setStatus(String(e), true);
  } finally {
    setBusy(false);
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  const win = getCurrentWindow();

  applyIntensityFromSettings();
  await restoreWindowPosition();
  // 启动时清掉历史尺寸字段，避免旧版数据残留
  saveSettings({});

  el.intensity()?.addEventListener("input", () => {
    syncIntensityLabel();
    saveSettings({ intensity: getIntensity() });
  });

  await win.onMoved(() => scheduleSaveWindowPosition());

  await listen<ProgressEvent>("compress-progress", (event) => {
    updateProgress(event.payload);
  });

  el.btnCancel()?.addEventListener("click", () => {
    void invoke("cancel_batch");
    setStatus("正在取消剩余任务…");
  });

  el.btnCopyFail()?.addEventListener("click", async () => {
    const text = [
      ...lastFailures.map((f) => `失败\t${f.path || f.error}`),
      ...lastSkips.map((f) => `跳过\t${f.path}\t${f.error}`),
    ].join("\n");
    if (!text) {
      setStatus("没有失败/跳过项可复制", true);
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      setStatus("失败/跳过信息已复制");
    } catch {
      setStatus("复制失败，请手动选择列表文本", true);
    }
  });

  await win.onDragDropEvent((event) => {
    if (event.payload.type === "over") {
      el.drop()?.classList.add("hover");
    } else if (event.payload.type === "leave" || event.payload.type === "drop") {
      el.drop()?.classList.remove("hover");
    }

    if (event.payload.type === "drop") {
      if (busy) return;
      const paths = event.payload.paths ?? [];
      if (!paths.length) {
        setStatus("未检测到文件", true);
        return;
      }
      void startBatch(paths);
    }
  });

  setStatus("就绪 · 拖入图片即可压缩");
});
