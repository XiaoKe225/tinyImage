import { invoke } from "@tauri-apps/api/core";
import { LogicalPosition } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import {
  availableMonitors,
  getCurrentWindow,
  type Monitor,
} from "@tauri-apps/api/window";
import tipQrUrl from "./assets/tip-qr.png";

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
  alwaysOnTop?: boolean;
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
  btnTip: () => document.getElementById("btn-tip") as HTMLButtonElement | null,
  btnPin: () => document.getElementById("btn-pin") as HTMLButtonElement | null,
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
      alwaysOnTop: o.alwaysOnTop === true,
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
    alwaysOnTop:
      partial.alwaysOnTop !== undefined ? partial.alwaysOnTop : cur.alwaysOnTop,
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

/** 标题栏至少与某块显示器工作区有足够重叠，才算「找得到窗口」 */
function isLogicalPosOnAnyMonitor(
  lx: number,
  ly: number,
  winW: number,
  winH: number,
  monitors: Monitor[],
): boolean {
  const titleH = Math.min(48, winH);
  for (const m of monitors) {
    const sf = m.scaleFactor || 1;
    const wx = m.workArea.position.x / sf;
    const wy = m.workArea.position.y / sf;
    const ww = m.workArea.size.width / sf;
    const wh = m.workArea.size.height / sf;
    const overlapW =
      Math.max(0, Math.min(lx + winW, wx + ww) - Math.max(lx, wx));
    const overlapH =
      Math.max(0, Math.min(ly + titleH, wy + wh) - Math.max(ly, wy));
    if (overlapW >= 80 && overlapH >= 20) return true;
  }
  return false;
}

/**
 * 启动可见性：恢复上次位置；若已跑到屏外（换屏/分辨率）则居中并清坏坐标；强制显示+聚焦。
 * 不改变「只持久化位置、不持久化大小」的已拍板口径。
 */
async function ensureWindowVisible() {
  const win = getCurrentWindow();
  try {
    await win.unminimize();
    await win.show();
  } catch {
    /* ignore */
  }

  let sizeW = 360;
  let sizeH = 360;
  try {
    const outer = await win.outerSize();
    const factor = await win.scaleFactor();
    sizeW = Math.max(1, Math.round(outer.width / factor));
    sizeH = Math.max(1, Math.round(outer.height / factor));
  } catch {
    /* 用默认最小窗 */
  }

  const s = loadSettings();
  const hasPos = typeof s.x === "number" && typeof s.y === "number";
  let placed = false;

  if (hasPos) {
    try {
      const monitors = await availableMonitors();
      if (
        monitors.length &&
        isLogicalPosOnAnyMonitor(s.x!, s.y!, sizeW, sizeH, monitors)
      ) {
        await win.setPosition(new LogicalPosition(s.x!, s.y!));
        placed = true;
      }
    } catch {
      /* 校验失败则走居中 */
    }
  }

  if (!placed) {
    try {
      await win.center();
    } catch {
      /* ignore */
    }
    if (hasPos) {
      try {
        const cur = loadSettings();
        const preserved: AppSettings = { intensity: cur.intensity };
        if (cur.alwaysOnTop) preserved.alwaysOnTop = true;
        localStorage.setItem(SETTINGS_KEY, JSON.stringify(preserved));
      } catch {
        /* ignore */
      }
    }
  }

  try {
    await win.setFocus();
  } catch {
    /* ignore */
  }
}

function syncPinButton(on: boolean) {
  const btn = el.btnPin();
  if (!btn) return;
  btn.classList.toggle("active", on);
  btn.setAttribute("aria-pressed", on ? "true" : "false");
  btn.setAttribute("aria-label", on ? "已置顶（点击取消）" : "窗口置顶");
  btn.title = on ? "已置顶（点击取消）" : "窗口置顶";
}

/** 恢复/应用图钉置顶（本机记住） */
async function applyAlwaysOnTopFromSettings() {
  const want = loadSettings().alwaysOnTop === true;
  try {
    await getCurrentWindow().setAlwaysOnTop(want);
    syncPinButton(want);
  } catch {
    syncPinButton(false);
  }
}

async function toggleAlwaysOnTop() {
  const win = getCurrentWindow();
  let next = false;
  try {
    const cur = await win.isAlwaysOnTop();
    next = !cur;
    await win.setAlwaysOnTop(next);
  } catch {
    setStatus("置顶切换失败", true);
    return;
  }
  saveSettings({ alwaysOnTop: next });
  syncPinButton(next);
  setStatus(next ? "已置顶" : "已取消置顶");
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

/** 打赏：本地收款码弹窗（离线资源，零网络） */
function openTipModal() {
  const modal = document.getElementById("tip-modal");
  const img = document.getElementById("tip-qr") as HTMLImageElement | null;
  const closeBtn = document.getElementById("tip-close");
  const backdrop = document.getElementById("tip-backdrop");
  if (!modal || !img || !closeBtn) return;
  img.src = tipQrUrl;
  modal.hidden = false;
  const finish = () => {
    modal.hidden = true;
    closeBtn.removeEventListener("click", finish);
    backdrop?.removeEventListener("click", finish);
    document.removeEventListener("keydown", onKey);
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") finish();
  };
  closeBtn.addEventListener("click", finish);
  backdrop?.addEventListener("click", finish);
  document.addEventListener("keydown", onKey);
  closeBtn.focus();
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
  await ensureWindowVisible();
  await applyAlwaysOnTopFromSettings();
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

  el.btnTip()?.addEventListener("click", () => openTipModal());
  el.btnPin()?.addEventListener("click", () => {
    void toggleAlwaysOnTop();
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
