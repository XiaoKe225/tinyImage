/**
 * Minimal CDP Runtime.evaluate helper for local UI verification (WebView2).
 * Usage: node scripts/cdp-eval.mjs <port> "<js expression>"
 */
const port = process.argv[2] || "9333";
const expression = process.argv[3];
if (!expression) {
  console.error("USAGE: node cdp-eval.mjs <port> \"<expression>\"");
  process.exit(2);
}

const listRes = await fetch(`http://127.0.0.1:${port}/json/list`);
if (!listRes.ok) {
  console.error(`CDP_LIST_FAIL ${listRes.status}`);
  process.exit(3);
}
const targets = await listRes.json();
const page = targets.find((t) => t.type === "page" && t.webSocketDebuggerUrl);
if (!page) {
  console.error("CDP_PAGE_NOT_FOUND");
  process.exit(4);
}

const ws = new WebSocket(page.webSocketDebuggerUrl);
let msgId = 0;
const pending = new Map();

function send(method, params = {}) {
  const id = ++msgId;
  ws.send(JSON.stringify({ id, method, params }));
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        reject(new Error(`CDP_TIMEOUT ${method}`));
      }
    }, 8000);
  });
}

ws.addEventListener("message", (ev) => {
  const data = JSON.parse(ev.data);
  if (data.id && pending.has(data.id)) {
    const { resolve, reject } = pending.get(data.id);
    pending.delete(data.id);
    if (data.error) reject(new Error(data.error.message || "CDP_ERROR"));
    else resolve(data.result);
  }
});

await new Promise((resolve, reject) => {
  ws.addEventListener("open", resolve, { once: true });
  ws.addEventListener("error", () => reject(new Error("CDP_WS_ERROR")), { once: true });
});

await send("Runtime.enable");
const result = await send("Runtime.evaluate", {
  expression,
  returnByValue: true,
  awaitPromise: true,
});
ws.close();

const value = result?.result?.value;
if (value === undefined || value === null) {
  console.error("CDP_NULL");
  process.exit(5);
}
process.stdout.write(String(value));
