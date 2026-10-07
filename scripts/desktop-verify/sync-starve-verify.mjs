// R7 真机验证：附件上传失败不能饿死拉取（scripts/desktop-verify）
//
// 背景（REVIEW-2026-10-06.md R7）：`sync.rs` 原先的顺序是
// `push → blobs_up → pull → blobs_down`，而 `blobs_up` 里的
// `api.put_blob(&bytes)?` 失败会**中止整轮**。于是一张在慢上行里传不完的
// 25 MB 图，会让 pull **永远轮不到**，下一轮又卡在同一个附件上 ——
// "推送能出去、拉取停摆"，用户只看到一个附件上传错误。
// 修复是调序：pull 提到 blobs_up 之前（浏览体验优先于推送体验）。
//
// 怎么在真机上分辨真假：让替身服务端**只让上传失败**（`FAIL_BLOB=1`，
// `/api/blob` 返 500），而 pull 正常返回一个带可辨认标记的变更。
// 于是判据非常干净：
//   · 界面上出现 R7-PULL-MARKER -> ✓ 上传失败没有挡住拉取
//   · 一直不出现                 -> !! pull 被饿死了
//
// 为什么这必须真机验：`blobs_up` 的 `?` 与调序都在 Rust 里，cargo test
// 也有 `a_failing_blob_upload_does_not_starve_the_pull` 守着；真机这一遍
// 验的是**同一条真实链路**（真实 HTTP 500 -> 真实 put_blob 报错 ->
// 真实一轮 sync），而不是替身 mock。
//
// 前置：替身服务端以 FAIL_BLOB=1 起；桌面端带 CDP 端口起。
// 用法：node sync-starve-verify.mjs

const CDP = "http://127.0.0.1:9222";
const FAKE = "http://127.0.0.1:8787";
const TOKEN = "sse-verify-token-0123456789abcdefghijklmnop";
const TIMEOUT_MS = 60_000;
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
    const timer = setTimeout(() => reject(new Error("evaluate 超时")), 10_000);
    pending.set(id, (msg) => {
      clearTimeout(timer);
      if (msg.result?.exceptionDetails) {
        return reject(new Error("表达式抛异常: " + JSON.stringify(msg.result.exceptionDetails.exception)));
      }
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

// 等 Tauri 注入（刚启动时还没注入，误判会变成"这是张空页"）
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

// 确认替身服务端确实开着"上传失败"开关 —— 否则这个验证什么也证明不了。
// runId 每次启动都不同，并被编进拉取消息的正文：这是**唯一可靠的判读依据**。
// 用固定的 "R7-PULL-MARKER" 会踩过坑：上一轮的残留消息还留在库里，
// 脚本会拿着残留判一个假通过（实测过一轮 pulled:0 却"通过"）。
const statsBefore = await (await fetch(`${FAKE}/__stats`)).json();
if (!statsBefore.failBlob) {
  console.error("替身服务端没开 FAIL_BLOB=1，上传不会失败，这个验证不成立。");
  process.exit(1);
}
const RUN_ID = statsBefore.runId;
console.log(`已确认：替身服务端 /api/blob 会返回 500（上传必然失败）`);
console.log(`本轮 runId: ${RUN_ID}`);

// 造一条**本地的**附件，让上传队列里真有东西。
// 用应用自己的写命令，走的是真实 IPC 接缝。
const marker = `R7 本地附件 ${Date.now()}`;
const bytes = Array.from({ length: 512 }, (_, i) => i % 256);
const sha = await evaluate(`(async () => {
  const sha = await window.__TAURI_INTERNALS__.invoke('save_attachment', { bytes: ${JSON.stringify(bytes)} });
  return sha;
})()`);
if (!sha || typeof sha !== "string") {
  console.error("save_attachment 没返回 sha，造不出待上传附件");
  process.exit(1);
}
console.log(`已写入本地附件: ${sha.slice(0, 12)}…（${bytes.length} 字节）`);
console.log(`标记: ${marker}`);

// 判读**直接查库**（list_timeline），不去看界面文本。
//
// 为什么不能看界面：这一轮同步的**最终状态是失败**（上传 500 让整轮返回
// Err），前端拿到失败状态就不会重载时间线 —— 于是"库里有了、界面上没出现"。
// 拿界面当判据会把这个正确的行为误判成"pull 被饿死"（实测踩过一轮：
// 服务端记录 pull 成功、库里也确实有那条消息，界面却因为状态是失败而没刷新）。
//
// R7 要验的是"拉取有没有执行"，那是数据层的事，所以就在数据层断言。
const pulled = () =>
  evaluate(`(async () => {
     const p = await window.__TAURI_INTERNALS__.invoke('list_timeline', { scope: 'all', limit: 500 });
     return JSON.stringify((p.items || []).some(m => (m.body || '').includes(${JSON.stringify(RUN_ID)})));
   })()`);

const before = await pulled();
console.log(`触发之前库里能查到 ${RUN_ID} 吗: ${before}`);
if (before === "true") {
  console.error("本轮标记一开始就在库里，观察无效。请重启替身服务端换一个 runId。");
  process.exit(1);
}

// 触发一轮同步
const t0 = Date.now();
await evaluate(`window.__TAURI_INTERNALS__.invoke('sync_now').then(()=>'triggered', e=>'err:'+e)`);
console.log("已触发 sync_now，开始等本轮拉取的消息进库…");

let seen = false;
while (Date.now() - t0 < TIMEOUT_MS) {
  if ((await pulled()) === "true") {
    seen = true;
    break;
  }
  await new Promise((r) => setTimeout(r, POLL_MS));
}

const elapsed = Date.now() - t0;
const status = await evaluate(
  "(async () => JSON.stringify(await window.__TAURI_INTERNALS__.invoke('get_sync_status')))()"
);
const stats = await (await fetch(`${FAKE}/__stats`)).json();
ws.close();

console.log(`\n等了 ${Math.round(elapsed / 1000)} 秒`);
console.log(`同步状态: ${status}`);
console.log(`替身服务端: blob 上传尝试 ${stats.blobTries} 次（全部 500），pull 成功 ${stats.pullOk} 次`);

if (seen) {
  console.log(`\n判定: ✓ 上传全程 500 的情况下，拉取的消息进了本地库（${RUN_ID}）—— R7 已修`);
  console.log(`（修复前 pull 排在 blobs_up 之后，会被那个 500 直接中止，永远轮不到。）`);
  console.log(`注意界面此时不会自动刷新：整轮同步的最终状态是失败，前端不重载时间线。`);
  process.exit(0);
}
console.log(`\n判定: !! 上传失败后 ${TIMEOUT_MS / 1000} 秒内拉取的消息始终没进库 —— pull 被饿死了`);
process.exit(1);
