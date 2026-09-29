// 极简 CDP 求值器（scripts/desktop-verify）
//
// 用法: node cdp-eval.mjs <url正则> <要求值的JS表达式>
//
// url 参数按**正则**匹配，而且是第一个命中。主窗口和捕获浮层的 URL 分别是
// `http://tauri.localhost/` 和 `http://tauri.localhost/capture.html` —— 前者是
// 后者的前缀，所以想选中主窗口必须用结尾锚点：`tauri\.localhost/$`。
//
// 为什么要它：捕获浮层是个 WebView，截图那条路在 Tauri v2 上不可信
// （DirectComposition，CopyFromScreen 抓出来是空白 —— README 里已经写过）。
// CDP 是**能看见真相**的那条路：它直接读 DOM。
//
// Node 24 自带全局 WebSocket，不需要任何依赖。

const [, , urlPattern, expression] = process.argv;
if (!urlPattern || !expression) {
  console.error("用法: node cdp-eval.mjs <url正则> <JS表达式>");
  process.exit(2);
}

const rx = new RegExp(urlPattern);
const targets = await (await fetch("http://127.0.0.1:9222/json/list")).json();
const target = targets.find((t) => rx.test(t.url || ""));
if (!target) {
  console.error(`没找到 url 匹配 /${urlPattern}/ 的 target。现有：`);
  for (const t of targets) console.error(`  ${t.type}  ${t.url}`);
  process.exit(1);
}

const ws = new WebSocket(target.webSocketDebuggerUrl);
const result = await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("CDP 超时")), 10000);
  ws.onopen = () => {
    ws.send(
      JSON.stringify({
        id: 1,
        method: "Runtime.evaluate",
        params: { expression, returnByValue: true, awaitPromise: true },
      })
    );
  };
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id !== 1) return;
    clearTimeout(timer);
    resolve(msg);
  };
  ws.onerror = (e) => {
    clearTimeout(timer);
    reject(new Error(`WS 出错: ${e.message ?? e}`));
  };
});
ws.close();

if (result.error) {
  console.error("CDP 错误:", JSON.stringify(result.error));
  process.exit(1);
}
const r = result.result?.result;
if (result.result?.exceptionDetails) {
  console.error("表达式抛异常:", JSON.stringify(result.result.exceptionDetails.exception));
  process.exit(1);
}
console.log(typeof r?.value === "string" ? r.value : JSON.stringify(r?.value, null, 2));
