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
//
// 2026-09 补了筛选导出。它把"命令接缝"这条理由顶到了最前面：`filter` 是个
// **对象**，字段名经 IPC 变成 camelCase（`channelId` / `fromMs` / `toMs`），
// 写错一个字母，`cargo test` 全绿而实机得到一句"缺少参数"。
// 筛选那几条断言刻意**把区间端点取在真实记录的 created_at 上** ——
// 端点含不含在内，只有在这种取法下才验得出来。

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// 工作目录默认落在仓库根的 `.scratch/export-verify/` 下（gitignore 的临时区）。
// **用脚本位置推仓库根，不写死路径** —— 否则换台机器就废了。
// 第一个参数可以覆盖它（想跑两次对比时有用）。
const WORK =
  process.argv[2] ||
  fileURLToPath(new URL("../../.scratch/export-verify/", import.meta.url));
// 每次跑用**自己的目录**。
//
// 原来所有运行都往 `out/` 里写，于是"摘要里的条数 == 目录里的文件数"那条断言
// 只在**第一次**跑得通：第二次跑时目录里还留着上次的文件，而摘要数的是本次。
// 那种红不是产品的问题，却会让人以为导出坏了。分目录之后整套断言可以反复跑。
//
// 目录名报给 `export-db.py`（`result.json` 里的 `dirs`），那边不再自己拼。
const RUN = "run-" + Date.now();
const OUT_DIR = path.join(WORK, RUN, "out");
// 筛选导出的几个落点分开，断言才能各数各的
const OUT_CHANNEL_TAG = path.join(WORK, RUN, "out-filter-channel-tag");
const OUT_TAG_ONLY = path.join(WORK, RUN, "out-filter-tag");
const OUT_RANGE = path.join(WORK, RUN, "out-filter-range");
const OUT_EMPTY = path.join(WORK, RUN, "out-filter-empty");

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
/**
 * 专门给"按标签筛"用的标签名，同样带本次运行的标记。
 *
 * 前缀**刻意和 TAG 不同**（而不是 `"筛" + TAG`）：这些记录也会被全量导出，
 * 而 `export-db.py` 用"正文里有没有 TAG"来认领"本次运行写下的那两条"。
 * 名字里嵌了 TAG 的话，全量导出那一节的计数会凭空多出两条。
 */
const FILTER_TAG = "filtercase-" + Date.now();

const expr = `(async () => {
  const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  if (!inv) return { fatal: "window.__TAURI_INTERNALS__.invoke 不存在" };

  const out = { tag: ${JSON.stringify(TAG)} };

  // ---- 造数据：频道 + 附件 + 两条记录 ----
  const ch = await inv("create_channel", { name: "项目A " + ${JSON.stringify(TAG)} });
  out.channel = ch.name;
  out.channelId = ch.id;

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

  // 一条引用了**本地还没有的**附件的记录。
  //
  // 摘要里那个 missingAttachments 得有东西可数：原先这条断言靠的是"用户的真
  // 实库里碰巧有个悬空引用"，换一份干净的库就红了 —— 那是断言在依赖环境，
  // 不是产品。这里自己造一个，断言才自足。
  // 正文**不带本次标记**：它属于全量导出那一节的计数，不该混进"本次那两条"。
  await inv("append_message", {
    body: "dangling-ref-case attachment:" + "d".repeat(64),
    channelId: null,
  });

  // ---- 全量导出 ----
  out.export = await inv("export_markdown", {
    dir: ${JSON.stringify(OUT_DIR)},
    utcOffsetMinutes: 480,
  });

  // ---- 筛选导出用的两条：同样的标签，一个在频道里、一个在收件箱 ----
  const m3 = await inv("append_message", {
    body: ${JSON.stringify(TAG)} + " 只有这条该被频道+标签筛出来",
    channelId: ch.id,
  });
  await inv("set_message_tags", { messageId: m3.id, tags: [${JSON.stringify(FILTER_TAG)}] });

  const m4 = await inv("append_message", {
    body: ${JSON.stringify(TAG)} + " 在收件箱、但带同一个标签",
    channelId: null,
  });
  await inv("set_message_tags", { messageId: m4.id, tags: [${JSON.stringify(FILTER_TAG)}] });

  out.filterTag = ${JSON.stringify(FILTER_TAG)};
  // 区间端点**取在真实记录的时刻上**：下界取 m1、上界取 m3。
  // 端点若写成开区间，这两条里就会少一条 —— 这正是要验的东西。
  out.rangeFrom = m1.createdAt;
  out.rangeTo = m3.createdAt;

  // ---- 频道 × 标签：交集，只该出 m3 ----
  out.filterChannelTag = await inv("export_markdown", {
    dir: ${JSON.stringify(OUT_CHANNEL_TAG)},
    utcOffsetMinutes: 480,
    filter: { channelId: ch.id, tag: ${JSON.stringify(FILTER_TAG)} },
  });

  // ---- 只按标签：m3 + m4，落在两个不同的目录里 ----
  out.filterTagOnly = await inv("export_markdown", {
    dir: ${JSON.stringify(OUT_TAG_ONLY)},
    utcOffsetMinutes: 480,
    filter: { tag: ${JSON.stringify(FILTER_TAG)} },
  });

  // ---- 频道 + 闭区间：m1 与 m3 都该在 ----
  out.filterRange = await inv("export_markdown", {
    dir: ${JSON.stringify(OUT_RANGE)},
    utcOffsetMinutes: 480,
    filter: {
      channelId: ch.id,
      fromMs: m1.createdAt,
      toMs: m3.createdAt,
    },
  });

  // ---- 上界落在过去：一条都不该有（空结果是一句回答，不是错误）----
  out.filterEmpty = await inv("export_markdown", {
    dir: ${JSON.stringify(OUT_EMPTY)},
    utcOffsetMinutes: 480,
    filter: { channelId: ch.id, toMs: m1.createdAt - 1 },
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

// 导出对话框那一段：菜单 → 面板 → 控件。
//
// **到"选择目录并导出"为止**：那个按钮会弹 Windows 原生选目录对话框，CDP 驱动
// 不了（已知边界，见 desktop-verify/README.md）。所以这里验的是**面板本身**：
// 入口点得到、控件齐、下拉里是真实的频道和标签、日期写反了会被拦住。
//
// 开头先**用界面自己的路径**造一条频道、发一条消息：面板和侧边栏读的是同一份
// 状态（`App` 的 `channels` / `tags`），而这份状态是挂载时拉、之后靠操作触发重取的。
// 只调命令写库的话界面根本不知道，下拉里自然什么也没有 —— 那是**测法**的问题，
// 不是面板的问题。顺带也就验到了"面板拿的是当前状态"。
const uiExpr = `(async () => {
  const out = {};
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  // React 记着自己写进表单控件的值，直接 .value = x 它收不到；要走原生 setter
  const setValue = (el, v) => {
    const proto =
      el instanceof HTMLSelectElement ? HTMLSelectElement.prototype
      : el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value").set.call(el, v);
    el.dispatchEvent(new Event("change", { bubbles: true }));
    el.dispatchEvent(new Event("input", { bubbles: true }));
  };
  // 元素的**纯文本**（跳过计数那个 span）——标签 chip 的 textContent 会把数字带上
  const text = (el) =>
    [...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.nodeValue).join("").trim();

  // ---- 界面造数据：新建一个频道（侧边栏 ＋ → 回车）----
  const plus = document.querySelector(".section-head .icon-btn");
  if (!plus) return { fatal: "找不到侧边栏的 ＋ 按钮" };
  click(plus);
  await sleep(150);
  const inline = document.querySelector(".inline-input");
  if (!inline) return { fatal: "点了 ＋ 却没有出现输入框" };
  const channelName = "面板验证" + Date.now();
  setValue(inline, channelName);
  inline.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  await sleep(800);

  // ---- 再发一条消息：发完界面会重取元数据（频道 / 标签计数）----
  const ta = document.querySelector(".composer-input");
  const send = document.querySelector(".send-btn");
  if (ta && send) {
    setValue(ta, "面板验证用的这一条");
    click(send);
    await sleep(1200);
  }
  out.sent = document.querySelector(".composer-input")?.value === "";

  // ---- 侧边栏（面板该和它显示同一批频道 / 标签）----
  out.sidebarChannels = [...document.querySelectorAll(".sidebar .nav-item.group .nav-name")].map(
    (e) => e.textContent.replace(/^#/, "")
  );
  out.sidebarTags = [...document.querySelectorAll(".sidebar .tag-chip")].map(text);

  // ---- ⋯ → 导出记录… ----
  const menuBtn = document.querySelector(".overflow-btn");
  if (!menuBtn) return { ...out, fatal: "找不到 ⋯ 按钮" };
  click(menuBtn);
  await sleep(150);

  const item = [...document.querySelectorAll(".overflow-item")].find((b) =>
    b.textContent.includes("导出记录")
  );
  out.menuItem = item ? item.textContent.trim() : null;
  if (!item) return out;
  click(item);
  await sleep(250);

  const modal = document.querySelector(".modal");
  out.modalTitle = modal ? modal.querySelector("h2").textContent : null;
  if (!modal) return out;

  const selects = modal.querySelectorAll("select");
  const dates = modal.querySelectorAll('input[type="date"]');
  out.selects = selects.length;
  out.dates = dates.length;
  // 选项文本形如「项目A（3）」——括号里是计数，比较的时候去掉
  const optionTexts = (sel) => [...(sel?.options ?? [])].map((o) => o.textContent.replace(/（\\d+）$/, ""));
  out.channelOptions = optionTexts(selects[0]);
  out.tagOptions = optionTexts(selects[1]);

  // 起 > 止：面板该当场拦住，而不是导出一棵空目录树
  setValue(dates[0], "2030-01-02");
  setValue(dates[1], "2020-01-02");
  await sleep(150);
  out.guard = [...modal.querySelectorAll(".note.err")].map((n) => n.textContent);
  out.exportDisabled = modal.querySelector(".btn.primary").disabled;

  // 恢复正常顺序：拦住的状态该消失
  setValue(dates[0], "2020-01-02");
  setValue(dates[1], "2030-01-02");
  await sleep(150);
  out.guardAfterFix = [...modal.querySelectorAll(".note.err")].map((n) => n.textContent);
  out.exportEnabledAfterFix = !modal.querySelector(".btn.primary").disabled;

  // 关掉面板：别把它留在界面上影响后面的人工检查
  const close = [...modal.querySelectorAll(".btn")].find((b) => b.textContent.includes("关闭"));
  if (close) click(close);
  await sleep(150);
  out.modalClosed = document.querySelector(".modal") === null;

  return out;
})()`;

/** 一次 `Runtime.evaluate`，等结果回来。 */
async function evaluate(expression, timeoutMs = 60000) {
  const id = nextId++;
  const msg = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("CDP 超时")), timeoutMs);
    ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id !== id) return;
      clearTimeout(timer);
      resolve(m);
    };
    ws.send(
      JSON.stringify({
        id,
        method: "Runtime.evaluate",
        params: { expression, returnByValue: true, awaitPromise: true },
      })
    );
  });
  if (msg.result?.exceptionDetails) {
    console.error("页面上抛异常：");
    console.error(JSON.stringify(msg.result.exceptionDetails, null, 2));
    process.exit(1);
  }
  return msg.result?.result?.value;
}

let nextId = 1;

const v = await evaluate(expr);
if (v?.fatal) {
  console.error("!! " + v.fatal);
  process.exit(1);
}

// 界面那一段单独跑：它不该因为导出断言失败而被跳过，所以结果分开存。
v.ui = await evaluate(uiExpr);
ws.close();

if (v.ui?.fatal) {
  console.error("!! 界面： " + v.ui.fatal);
  process.exit(1);
}

// 顺手把摘要落盘，给 export-db.py 用 —— 这样"跑一次验证"只需要两条命令，
// 中间不必再拿 shell 重定向去接 stdout。
//
// 目录名也一起报过去：那边**不自己拼路径**，否则两处各写一份，改了一处就会
// 悄悄去看上一个运行留下的旧目录（然后一切"通过"）。
v.dirs = {
  work: WORK,
  run: path.join(WORK, RUN),
  out: OUT_DIR,
  channelTag: OUT_CHANNEL_TAG,
  tagOnly: OUT_TAG_ONLY,
  range: OUT_RANGE,
  empty: OUT_EMPTY,
};
fs.mkdirSync(WORK, { recursive: true });
fs.writeFileSync(path.join(WORK, "result.json"), JSON.stringify(v, null, 2));
console.log(JSON.stringify(v, null, 2));
