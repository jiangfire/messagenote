// 单条复制的端到端验证（scripts/desktop-verify）
//
// 验的是"复制出去的东西是不是**自包含**的"：走应用自己的写命令造一条带图的记录，
// 调 `render_message_markdown`（就是 ⧉ 按钮调的那个命令），然后**在 Node 这边
// 独立地把 data URI 解回来**，和存进去的字节逐字节比。
//
// 为什么不在页面里解：页面里解等于让被测代码自己证明自己。
// 只断言"有个 data:image/png;base64," 更是弱断言 —— base64 编码错了它照样通过。

const PNG = [137, 80, 78, 71, 13, 10, 26, 10, ...Array(24).fill(0)];

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

const expr = `(async () => {
  const inv = window.__TAURI_INTERNALS__.invoke;
  const PNG = ${JSON.stringify(PNG)};

  const ch = await inv("create_channel", { name: "复制验证" });
  const sha = await inv("save_attachment", { bytes: PNG });
  const m = await inv("append_message", {
    body: "看这个 ![截图](attachment:" + sha + ")",
    channelId: ch.id,
  });
  const md = await inv("render_message_markdown", { id: m.id, utcOffsetMinutes: 480 });

  // 也验一下"字节本地没有"的情形：引用一个库里根本没有的 sha
  const m2 = await inv("append_message", {
    body: "缺图 ![x](attachment:" + "f".repeat(64) + ")",
    channelId: ch.id,
  });
  const md2 = await inv("render_message_markdown", { id: m2.id, utcOffsetMinutes: 480 });

  return { sha, md, md2 };
})()`;

const msg = await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("CDP 超时")), 60000);
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id !== 1) return;
    clearTimeout(timer);
    resolve(m);
  };
  ws.send(
    JSON.stringify({
      id: 1,
      method: "Runtime.evaluate",
      params: { expression: expr, returnByValue: true, awaitPromise: true },
    })
  );
});
ws.close();

if (msg.result?.exceptionDetails) {
  console.error("页面上抛异常：", JSON.stringify(msg.result.exceptionDetails, null, 2));
  process.exit(1);
}
const { sha, md, md2 } = msg.result.result.value;

const fails = [];
const check = (cond, what) => {
  console.log(`  ${cond ? "✓" : "!!"} ${what}`);
  if (!cond) fails.push(what);
};

console.log("=== 单条复制：带图那条 ===");
console.log(`  sha: ${sha}`);
check(md.includes("---\n"), "带 front-matter（和导出同一个渲染器）");
check(!md.includes("attachment:"), "正文里**没有**别的程序看不懂的 attachment: 引用");
check(md.includes("data:image/png;base64,"), "图被内联成 data URI");

const b64 = md.split("base64,")[1]?.split(")")[0];
check(!!b64, "能取到 base64 载荷");
if (b64) {
  const decoded = Buffer.from(b64, "base64");
  // **最强的一条**：在 Node 这边独立解码，和存进去的字节逐字节比。
  check(decoded.equals(Buffer.from(PNG)), "独立解回来的字节和存进去的逐字节一致");
  check(decoded.length === PNG.length, `长度一致（${decoded.length}）`);
}

console.log();
console.log("=== 字节不在本地时 ===");
check(md2.includes("attachment:" + "f".repeat(64)), "保留原引用（编不出 data URI，原引用至少诚实）");

console.log();
if (fails.length) {
  console.log(`!! ${fails.length} 条断言没过`);
  process.exit(1);
}
console.log("全部通过");
