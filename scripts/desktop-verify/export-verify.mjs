// 导出功能的端到端验证（scripts/desktop-verify）
//
// 走的是**真实的那条路**：
//   1. 用应用自己的写命令造数据（create_channel / save_attachment /
//      append_message / set_message_tags）—— 不是往库里塞 SQL，
//      这样 FTS 索引和 HLC 都由应用自己维护，和用户真用出来的数据一样。
//   2. 调 `export_markdown` 导出到一个真实目录。
//   3. 调 `render_message_markdown`（单条复制那条路）。
//
// 为什么必须实机跑：**命令名和参数名只有实机才验得到**。Rust 那边是
// `utc_offset_minutes`，Tauri v2 传到 JS 是 `utcOffsetMinutes`，写错就得到一句
// "缺少参数" —— 而 `cargo test` 全绿，因为它根本没经过 IPC 这一层。
//
// 用 `window.__TAURI_INTERNALS__.invoke` 而不是点按钮：按钮会弹 Windows 原生
// 选目录对话框，那个没法用 CDP 驱动。**这一步验的是命令接缝，不是对话框**；
// 对话框那一段只能人点（已知边界，写在输出里）。

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// 工作目录默认落在仓库根的 `.scratch/export-verify/` 下（gitignore 的临时区）。
// **用脚本位置推仓库根，不写死路径** —— 否则换台机器就废了。
// 第一个参数可以覆盖它（想跑两次对比时有用）。
const WORK =
  process.argv[2] ||
  fileURLToPath(new URL("../../.scratch/export-verify/", import.meta.url));
const OUT_DIR = path.join(WORK, "out");

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

// 一个最小的、会被嗅探成 image/png 的字节串：PNG 的 8 字节签名 + 填充。
// `sniff_mime` 认的就是签名，而这里要验的是"字节原样到达磁盘"，
// 所以是不是一张能解码的图并不影响这条结论（字节相等比"能解码"更强）。
const PNG = [137, 80, 78, 71, 13, 10, 26, 10, ...Array(24).fill(0)];

// 每次跑都带一个唯一的标记。
//
// **为什么需要它**：这套脚本会真的往库里写数据（走的是应用自己的写命令），
// 所以第二次跑的时候库里还有上一次留下的东西 —— 断言里写死"一共 2 条"
// 会因为残留而失败，而那**不是**产品的问题。带上标记之后，断言只找属于
// 本次运行的文件，脚本因此可以反复跑。
const TAG = "verify-" + Date.now();

const expr = `(async () => {
  const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  if (!inv) return { fatal: "window.__TAURI_INTERNALS__.invoke 不存在" };

  const out = { tag: ${JSON.stringify(TAG)} };

  // ---- 造数据：频道 + 附件 + 两条记录 ----
  const ch = await inv("create_channel", { name: "项目A " + ${JSON.stringify(TAG)} });
  out.channel = ch.name;

  const sha = await inv("save_attachment", { bytes: ${JSON.stringify(PNG)} });
  out.sha = sha;

  const m1 = await inv("append_message", {
    body: ${JSON.stringify(TAG)} + " 会议记录：讨论下一步\\n\\n![截图](attachment:" + sha + ")",
    channelId: ch.id,
  });
  await inv("set_message_tags", { messageId: m1.id, tags: ["重要", "待办"] });
  out.messageId = m1.id;

  // 第二条故意落收件箱：验证"按频道分目录"时收件箱也是一个目录
  await inv("append_message", {
    body: ${JSON.stringify(TAG)} + " 地铁里随手记的一句",
    channelId: null,
  });

  // ---- 全量导出 ----
  out.export = await inv("export_markdown", {
    dir: ${JSON.stringify(OUT_DIR)},
    utcOffsetMinutes: 480,
  });

  // ---- 单条复制那条路 ----
  out.one = await inv("render_message_markdown", {
    id: m1.id,
    utcOffsetMinutes: 480,
  });

  // 不存在的 id 应当回 null，而不是抛
  out.gone = await inv("render_message_markdown", {
    id: "不存在的-id",
    utcOffsetMinutes: 480,
  });

  return out;
})()`;

let nextId = 1;
const result = await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("CDP 超时")), 60000);
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id !== nextId) return;
    clearTimeout(timer);
    resolve(msg);
  };
  ws.send(
    JSON.stringify({
      id: nextId,
      method: "Runtime.evaluate",
      params: { expression: expr, returnByValue: true, awaitPromise: true },
    })
  );
});
ws.close();

if (result.result?.exceptionDetails) {
  console.error("页面上抛异常：");
  console.error(JSON.stringify(result.result.exceptionDetails, null, 2));
  process.exit(1);
}
const v = result.result?.result?.value;
if (v?.fatal) {
  console.error("!! " + v.fatal);
  process.exit(1);
}
// 顺手把摘要落盘，给 export-db.py 用 —— 这样"跑一次验证"只需要两条命令，
// 中间不必再拿 shell 重定向去接 stdout。
fs.mkdirSync(WORK, { recursive: true });
fs.writeFileSync(path.join(WORK, "result.json"), JSON.stringify(v, null, 2));
console.log(JSON.stringify(v, null, 2));
