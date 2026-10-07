// R8 真机验证：长命令跑在主线程时界面不能冻结（scripts/desktop-verify）
//
// 背景（REVIEW-2026-10-06.md R8）：`commands.rs` 里 27 个 `#[tauri::command]`
// 原本**全是同步函数**。Tauri v2 的同步命令在**主线程内联执行**，于是
// `test_sync_connection`（最长 30 秒）、`export_markdown`（不设上界的文件 I/O）、
// `save_attachment`（IPC 上反序列化约 100 MB 的 JSON 数字数组）跑的时候，
// 托盘点击、全局快捷键、窗口事件、capture 浮层的 IPC 全部排队 ——
// 表现是"测连接/导出期间整个界面冻住"。
// 修复是给这几条加 `#[tauri::command(async)]`，让它们离开主线程。
//
// 怎么在真机上分辨真假：**在长命令飞行期间，持续从 CDP 问界面一句廉价的话**
// （读 `document.title`），量它的响应延迟。
//   · 主线程被占 -> CDP 的 evaluate 排在队尾，延迟涨到与长命令同量级（秒级）
//   · 已 async    -> evaluate 立刻返回，延迟始终是毫秒级
//
// 判据用**中位数**而不是最大值：偶尔一次慢可能是 GC 或磁盘抖动，
// 中位数才代表"常态冻结 vs 常态流畅"。
//
// 为什么不合成按键：只读 DOM + 计时，不抢前台焦点，可以和别的验证共存。
//
// 前置：桌面端带 CDP 端口起。
// 用法：node mainthread-verify.mjs

const CDP = "http://127.0.0.1:9222";
const PROBES = 40; // 长命令飞行期间打多少次
const INTERVAL_MS = 100; // 探测间隔
// 长命令：走一次真实导出（写文件 I/O，不设上界），是最贴近用户感受的那条。
// 用应用自己的写命令，验的是真实 IPC 接缝。
const MAX_MEDIAN_MS = 400;

const targets = await (await fetch(`${CDP}/json/list`)).json();
const target = targets.find((t) => /tauri\.localhost\/$/.test(t.url || ""));
if (!target) {
  console.error("没找到主窗口 target。桌面端是否已带 --remote-debugging-port 启动？");
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

function evaluate(expression, timeoutMs = 10_000) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error("evaluate 超时"));
    }, timeoutMs);
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

// 等 Tauri 注入
let ready = false;
for (let i = 0; i < 20; i++) {
  const v = await evaluate("JSON.stringify(window.__TAURI_INTERNALS__ ? 'ok' : 'no-tauri')");
  if (v === '"ok"') {
    ready = true;
    break;
  }
  await new Promise((r) => setTimeout(r, 500));
}
if (!ready) {
  console.error("等了 10 秒页面里仍没有 __TAURI_INTERNALS__");
  process.exit(1);
}

// ---- 基线：没有长命令在跑时的探测延迟 ----
async function probeSeries(n) {
  const out = [];
  for (let i = 0; i < n; i++) {
    const t = Date.now();
    try {
      await evaluate("document.title.length", 3000);
      out.push(Date.now() - t);
    } catch (e) {
      out.push(-1); // 超时 = 界面真的没响应
    }
    await new Promise((r) => setTimeout(r, INTERVAL_MS));
  }
  return out;
}

const median = (xs) => {
  const s = xs.filter((x) => x >= 0).sort((a, b) => a - b);
  if (!s.length) return -1;
  return s[Math.floor(s.length / 2)];
};

console.log("先量基线（没有长命令在跑）…");
const baseline = await probeSeries(12);
console.log(`基线中位延迟: ${median(baseline)} ms`);

// ---- 起飞一条长命令 ----
//
// **用 test_sync_connection 打到替身服务端的慢端点，而不是 export_markdown。**
// 原因很实在：在一这个小库上 export_markdown 只要 33 毫秒 —— 它压根占不住
// 主线程，于是"没冻结"这个结论是**因为命令太短**，不是因为修好了。
// 真要占住主线程得是那种会挂好几秒的命令：test_sync_connection 打到延迟
// 8 秒的握手端点，正好也是 REVIEW 里点名的最长 30 秒那条。
//
// 仍然直接调命令验 IPC 接缝，不点面板（点面板会弹原生选目录对话框）。
//
// 注意 `utcOffsetMinutes` 那类坑：`test_sync_connection` 的参数是
// url / token，漏了只会得到一句"缺少参数"，而 cargo test 全绿。
const SLOW_MS = Number(process.env.SLOW_MS || 8000);
const outDir = `.scratch/r8-export-${Date.now()}`;
console.log(`\n起飞长命令：test_sync_connection（服务端故意延迟 ${SLOW_MS}ms）`);
const longCmd = evaluate(
  `(async () => {
     const t0 = Date.now();
     const r = await window.__TAURI_INTERNALS__.invoke('test_sync_connection', {
       url: 'http://127.0.0.1:8787',
       token: 'sse-verify-token-0123456789abcdefghijklmnop'
     });
     return JSON.stringify({ ms: Date.now() - t0, r });
   })()`,
  120_000
);
longCmd.catch(() => {}); // 长命令还在飞，先不 await

// 长命令飞行期间持续探测界面
await new Promise((r) => setTimeout(r, 150)); // 给它一点起飞时间
console.log(`长命令飞行中，连打 ${PROBES} 次探测…`);
const during = await probeSeries(PROBES);

// 长命令收尾
let longResult = "(未完成)";
try {
  longResult = await longCmd;
} catch (e) {
  longResult = `失败: ${e.message}`;
}
ws.close();

const medDuring = median(during);
const timeouts = during.filter((x) => x < 0).length;
const medBase = median(baseline);

console.log(`\n长命令自身耗时: ${longResult}`);
console.log(`基线中位延迟: ${medBase} ms`);
console.log(`飞行期间中位延迟: ${medDuring} ms（超时 ${timeouts} 次 / ${PROBES}）`);
console.log(`延迟样本: ${during.join(", ")}`);

if (medDuring < 0 || timeouts > PROBES / 4) {
  console.log(`\n判定: !! 长命令飞行期间界面大量无响应 —— 主线程被占，R8 没修好`);
  process.exit(1);
}
if (medDuring <= MAX_MEDIAN_MS) {
  console.log(`\n判定: ✓ 长命令飞行期间界面中位延迟 ${medDuring} ms（阈值 ${MAX_MEDIAN_MS}），没冻结，R8 已修`);
  console.log(`（修复前这条导出/测连接跑在主线程上，CDP 探测会排队到秒级。）`);
  process.exit(0);
}
console.log(`\n判定: ? 中位延迟 ${medDuring} ms 超过阈值 ${MAX_MEDIAN_MS} ms，需人工判断`);
process.exit(1);
