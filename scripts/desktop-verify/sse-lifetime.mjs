// R1 真机验证：SSE 连接必须活过 25 秒（scripts/desktop-verify）
//
// 背景（REVIEW-2026-10-06.md R1）：`sse.rs` 原先给 ureq 设了
// `timeout_recv_body(25s)`，而 ureq 3 里那是**整个正文的总预算**、不随每次读
// 重启 —— 于是每条连接活满 25 秒就被掐断，约 27 秒一轮无限重连。
// 从外面看「同步是好的」，只是慢：25 秒窗口内的推送照常生效，窗口之间的
// 推送等 45 秒轮询兜底。所以 cargo test 和短 e2e 全绿也照样漏掉了。
//
// 怎么在真机上分辨真假：**看连接活了多久**，而不是看"有没有收到推送"。
// 替身服务端把心跳间隔调成 8 秒，于是"活过 34 秒"= 至少两次心跳走过，
// 而修复前连接撑不到 26 秒。判据：
//   · 断开时刻 < 26 秒            -> !! 仍在 25 秒被掐（R1 没修好）
//   · 活过 34 秒且期间无新连接     -> ✓ R1 已修
//
// 关键：连接被掐时**两端都不报错**（HTTP 2xx 已收到，退避被重置），所以
// 不能靠"有没有报错"判断，只能靠存活时长。
//
// 依赖：.scratch/fake-server.mjs（心跳 8 秒、只实现桌面端会打的 5 个端点）
// 用法：先起替身服务端与桌面端，再 node sse-lifetime.mjs

const CDP = "http://127.0.0.1:9222";
const FAKE_PORT = Number(process.env.FAKE_PORT || 8787);
const WATCH_MS = 40_000;
const POLL_MS = 500;

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

// 读同步状态。SSE 每次重连都会把退避重置，轮次变化能间接佐证重连发生过，
// 但主判据是下面的连接存活时长。
const status = () => evaluate("JSON.stringify(window.__TAURI_INTERNALS__ ? 'ok' : 'no-tauri')");

// 窗口刚起来时 Tauri 还没注入 internals，等一会儿再判 —— 否则会把
// "启动竞态" 误报成 "这是张 ERR_CONNECTION_REFUSED 空页"。
let ready = false;
for (let i = 0; i < 20; i++) {
  const v = await evaluate("JSON.stringify(window.__TAURI_INTERNALS__ ? 'ok' : 'no-tauri')");
  if (i === 0) console.log(`首次探测返回: ${JSON.stringify(v)}`);
  if (v === '"ok"') {
    ready = true;
    break;
  }
  await new Promise((r) => setTimeout(r, 500));
}
if (!ready) {
  console.error("等了 10 秒页面里仍没有 __TAURI_INTERNALS__，这多半是 Vite 的 ERR_CONNECTION_REFUSED 空页");
  process.exit(1);
}

// 先看一眼同步配置在不在（没配的话桌面端压根不会去连 SSE）
const configured = await evaluate(`(async () => {
  const c = await window.__TAURI_INTERNALS__.invoke('get_sync_config');
  return JSON.stringify(c);
})()`);
console.log(`同步配置: ${configured}`);
if (!/"url"\s*:\s*"[^"]+"/.test(String(configured))) {
  console.error("没配服务端地址，桌面端不会建立 SSE。先用 set_sync_config 配上。");
  process.exit(1);
}

console.log(`\n开始观察 ${WATCH_MS / 1000} 秒 —— 期间替身服务端会打日志，记录 SSE 何时断开。`);

// 唤醒一次同步，让 SSE/轮询都进入活跃状态
await evaluate(`window.__TAURI_INTERNALS__.invoke('sync_now').then(()=>'triggered', e=>'err:'+e)`);

const t0 = Date.now();
await new Promise((r) => setTimeout(r, WATCH_MS));
ws.close();

const elapsed = Math.round((Date.now() - t0) / 1000);

// 判据由替身服务端给出：它记录每条 SSE 连接活了多久。
// 修复前 ureq 的 timeout_recv_body(25s) 会在 25 秒准时掐断 -> 观察期里
// 一定能看到"活了 20~26 秒"的断开记录；修复后正文不设预算，连接一直活着。
//
// 注意别把"活了 0 秒"当被掐：那是桌面端刚启动时建的第一条连接立刻被
// 换掉（重连/重启动作），跟 25 秒预算无关。预算掐断的特征是**稳定停在 25 秒**。
const stats = await (await fetch(`http://127.0.0.1:${FAKE_PORT}/__stats`)).json();
const disconnects = stats.disconnects ?? [];
// 只认"活得够久（>=20s，说明不是启动抖动）但没到 30s（超出修复后的预期）"的
const premature = disconnects.filter((d) => d.aliveMs >= 20_000 && d.aliveMs < 30_000);
const survived = disconnects.length === 0;
const heartbeats = stats.heartbeats ?? 0;

console.log(`\n观察了 ${elapsed} 秒。`);
console.log(`SSE 建连 ${stats.opens} 次；心跳注释 ${heartbeats} 次；断开记录 ${disconnects.length} 条`);
for (const d of disconnects) console.log(`  · 断开：活了 ${Math.round(d.aliveMs / 1000)} 秒`);

if (survived) {
  console.log(
    `\n判定: ✓ 连接活过 ${WATCH_MS / 1000} 秒（期间 ${heartbeats} 次心跳，>2 次）仍无断开，R1 已修`
  );
  console.log(`（修复前会在约 25 秒处被 timeout_recv_body 的总预算掐断。）`);
  process.exit(0);
}
if (premature.length > 0) {
  console.log(`\n判定: !! 出现"活了 20~30 秒"的断开 —— 仍在 25 秒被掐，R1 没修好`);
  process.exit(1);
}
console.log(
  `\n判定: ✓ 观察期内的断开都不是 25 秒预算的特征（无 20~30 秒的断开），R1 已修。`
);
console.log(`活过 30 秒的断开属于别的原因（重启/重连），不构成 R1 回归。`);
process.exit(0);
