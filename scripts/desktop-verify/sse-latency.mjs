// 量「服务端写一条 → 桌面端界面上出现」的延迟（scripts/desktop-verify）
//
// 这是在验 ROADMAP「验证债」的第三条：SSE 唤醒同步线程到底有没有真的生效。
//
// 为什么这条能分辨真假：桌面端同时有**两条**路径 ——
//   · SSE 推送（应该几秒内到）
//   · 45 秒轮询兜底（最坏 45 秒）
// 所以只要量出"多久出现"，就能知道是哪条路径在起作用。
// 断言：< 10 秒算 SSE 生效；≥ 40 秒说明只有轮询、SSE 其实没接上。
//
// 全程只用 CDP 读 DOM + 一个 HTTP 请求，**不合成任何按键、不抢焦点**
// （前两项验证要抢焦点，这一项不用）。

const BASE = "http://127.0.0.1:8787";
const TOKEN = "sse-verify-token-0123456789abcdefghijklmnop";
const MARKER = `SSE延迟验证-${Date.now()}`;
const TIMEOUT_MS = 70_000;
const POLL_MS = 250;

const targets = await (await fetch("http://127.0.0.1:9222/json/list")).json();
const target = targets.find((t) => /tauri\.localhost\/$/.test(t.url || ""));
if (!target) {
  console.error("没找到主窗口 target");
  process.exit(1);
}

const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((res, rej) => {
  ws.onopen = res;
  ws.onerror = () => rej(new Error("CDP WS 连不上"));
});

let nextId = 1;
const pending = new Map();
ws.onmessage = (ev) => {
  const msg = JSON.parse(ev.data);
  const p = pending.get(msg.id);
  if (p) {
    pending.delete(msg.id);
    p(msg);
  }
};

function evaluate(expression) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    const timer = setTimeout(() => reject(new Error("evaluate 超时")), 8000);
    pending.set(id, (msg) => {
      clearTimeout(timer);
      resolve(msg.result?.result?.value);
    });
    ws.send(
      JSON.stringify({
        id,
        method: "Runtime.evaluate",
        params: { expression, returnByValue: true, awaitPromise: true },
      })
    );
  });
}

const seen = () => evaluate(`document.body.innerText.includes(${JSON.stringify(MARKER)})`);

// 先确认它**还不存在** —— 否则"出现了"这个观察没有意义。
if (await seen()) {
  console.error("标记一开始就存在，测量无效");
  process.exit(1);
}
console.log(`标记: ${MARKER}`);
console.log("已确认：写入之前界面上没有它");

const t0 = Date.now();
const resp = await fetch(`${BASE}/api/message`, {
  method: "POST",
  headers: { "Content-Type": "application/json", Authorization: `Bearer ${TOKEN}` },
  body: JSON.stringify({ body: MARKER }),
});
console.log(`服务端写入: HTTP ${resp.status}（t0）`);
if (!resp.ok) {
  console.error("服务端写入失败:", await resp.text());
  process.exit(1);
}

let found = null;
while (Date.now() - t0 < TIMEOUT_MS) {
  if (await seen()) {
    found = Date.now() - t0;
    break;
  }
  await new Promise((r) => setTimeout(r, POLL_MS));
}

ws.close();

if (found === null) {
  console.log(`!! ${TIMEOUT_MS / 1000} 秒内界面上一直没出现 —— SSE 和轮询都没生效`);
  process.exit(1);
}

console.log(`\n延迟: ${found} ms`);
if (found < 10_000) {
  console.log("判定: ✓ SSE 推送生效（远快于 45 秒轮询）");
  process.exit(0);
}
console.log("判定: !! 像是只走了 45 秒轮询，SSE 没接上");
process.exit(1);
