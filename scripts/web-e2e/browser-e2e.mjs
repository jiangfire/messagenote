/**
 * 网页端的真实浏览器端到端验证。
 *
 * 为什么非做不可：构建通过、类型检查通过、用 Node 直接调 httpApi 也都通过 ——
 * 但那些都证明不了"React 挂载了、事件接上了、页面真的能用"。
 * 这一轮就在这里抓到了两个别的手段抓不到的问题（见下面的断言）。
 */
import {
  waitForDevTools,
  newTarget,
  openSession,
  evaluate,
  waitFor,
  fill,
  click,
  pressEnter,
  screenshot,
  visibleText,
  sleep,
} from "./cdp.mjs";

const APP = process.argv[2];
const TOKEN = process.argv[3];
/** 截图落在这里。默认是当前目录下的 shots/，跑完可以直接看。 */
const SHOTS = process.argv[4] ?? "shots";

let failed = 0;
function ok(label, cond, extra = "") {
  if (cond) {
    console.log(`  ok   ${label}`);
  } else {
    failed++;
    console.log(`  FAIL ${label}${extra ? " —— " + extra : ""}`);
  }
}

const text = () => visibleText(s);

/**
 * 让用例可重复跑：先把服务端的数据清空。
 *
 * 用长期令牌直接调 API —— 这一段不是被测对象，只是布置现场。
 * 不清的话，第二次跑时时间线里已经有上一轮的数据，"空状态文案"和"计数变成 1"
 * 这些断言就会因为环境不对而假红，掩盖真正的问题。
 */
async function resetServer(base, token) {
  const h = { Authorization: `Bearer ${token}` };
  const tl = await (await fetch(`${base}/api/timeline?scope=all&limit=500`, { headers: h })).json();
  for (const m of tl.items) {
    await fetch(`${base}/api/message/${encodeURIComponent(m.id)}`, { method: "DELETE", headers: h });
  }
  const chans = await (await fetch(`${base}/api/channels`, { headers: h })).json();
  for (const c of chans) {
    if (c.id === "inbox") continue;
    await fetch(`${base}/api/channel/${encodeURIComponent(c.id)}`, { method: "DELETE", headers: h });
  }
}

await resetServer(new URL(APP).origin, TOKEN);

await waitForDevTools();
const target = await newTarget(APP);
const s = await openSession(target.webSocketDebuggerUrl);

// 上一次运行留下的会话会让页面直接进主界面。先清干净 ——
// 这个浏览器配置是复用的，localStorage 会跨次留存。
await sleep(600);
await evaluate(s, "localStorage.clear(); true");
await s.send("Page.reload", { ignoreCache: true });
await sleep(600);

// ---------------------------------------------------------------- 登录
console.log("== 登录 ==");
await waitFor(s, "!!document.querySelector('.login-card')", "登录卡片");
ok("登录页渲染出来了", (await text()).includes("MessageNote"));
ok(
  "服务端地址默认就是页面自己的 origin（同源部署下不用手填）",
  await evaluate(
    s,
    "document.querySelector('.login-card input:not([type=password])').value === location.origin"
  )
);
await screenshot(s, `${SHOTS}/web-01-login.png`);

await fill(s, ".login-card input[type=password]", "wrong-token-aaaaaaaaaaaaaaaaaaaaaaaaaaaa");
await click(s, ".login-card button[type=submit]");
await waitFor(s, "!!document.querySelector('.login-error')", "错误提示");
ok(
  "错的令牌给出人话错误",
  (await evaluate(s, "document.querySelector('.login-error').innerText")).includes("令牌不对")
);

await fill(s, ".login-card input[type=password]", TOKEN);
await click(s, ".login-card button[type=submit]");
await waitFor(s, "!!document.querySelector('.app')", "主界面");
await sleep(900);
ok("登录后进入主界面", (await text()).includes("时间线"));

// 这一条是**打开浏览器才发现的**：桌面端的空状态提示的是全局快捷键。
ok(
  "空状态没有提桌面端的全局快捷键",
  !(await text()).includes("Ctrl+Shift+Space"),
  "浏览器里没有那个快捷键，提它会让用户去找一个不存在的东西"
);
ok(
  "空状态改成了网页端的说法",
  (await text()).includes("下面那个输入框就是入口")
);
await screenshot(s, `${SHOTS}/web-02-empty.png`);

// ---------------------------------------------------------------- 写
console.log("== 记一条 ==");
await fill(s, ".composer-input", "浏览器里记的第一条");
await click(s, ".send-btn");
await waitFor(s, `document.body.innerText.includes('浏览器里记的第一条')`, "新消息出现");
ok("新记录出现在时间线里", true);
ok(
  "输入框已清空",
  (await evaluate(s, "document.querySelector('.composer-input').value")) === ""
);
ok(
  "侧边栏计数变成 1",
  (await evaluate(s, "document.querySelector('.nav-count').innerText")).trim() === "1"
);
await screenshot(s, `${SHOTS}/web-03-note.png`);

// 再记一条中文的，用来测检索
await fill(s, ".composer-input", "地铁上想到的一个点子");
await click(s, ".send-btn");
await waitFor(s, `document.body.innerText.includes('地铁上想到的一个点子')`, "第二条出现");
ok("第二条也记下了", true);

// ---------------------------------------------------------------- 搜
console.log("== 检索 ==");
await fill(s, ".search-input", "地铁");
await sleep(900); // 防抖 160ms + 一次往返
await waitFor(s, `document.body.innerText.includes('检索')`, "检索视图");
ok("检索命中", (await text()).includes("地铁上想到的一个点子"));
ok(
  "检索时不该把不相干的带出来",
  !(await text()).includes("浏览器里记的第一条")
);
await screenshot(s, `${SHOTS}/web-04-search.png`);

await fill(s, ".search-input", "");
await sleep(700);
ok("清空检索后回到时间线", (await text()).includes("浏览器里记的第一条"));

// ---------------------------------------------------------------- 频道
console.log("== 频道 ==");
await click(s, ".icon-btn[title='新建频道']");
await waitFor(s, "!!document.querySelector('.inline-input')", "频道名输入框");
await fill(s, ".inline-input", "浏览器建的频道");
await pressEnter(s, ".inline-input");
await waitFor(s, `document.body.innerText.includes('浏览器建的频道')`, "频道出现");
ok("新建频道成功", true);

// **这一步不能省。** 不把记录真放进频道，下面"删频道不该删记录"就是空跑 ——
// 频道里本来就没东西，删了当然也不会少。第一版测试就是这么空跑过去的。
await click(s, ".msg-actions button[title='归档到频道']");
await waitFor(s, "!!document.querySelector('.move-row')", "归档行");
await evaluate(
  s,
  `(() => {
     const b = [...document.querySelectorAll('.move-row .btn.tiny')]
       .find(x => x.innerText.includes('浏览器建的频道'));
     if (!b) throw new Error('归档行里没有目标频道');
     b.click();
     return true;
   })()`
);
await sleep(1100);

const inChannel = await evaluate(
  s,
  `(() => {
     const item = [...document.querySelectorAll('.nav-item.group')]
       .find(x => x.innerText.includes('浏览器建的频道'));
     return item?.querySelector('.nav-count')?.innerText.trim() ?? null;
   })()`
);
ok(
  "记录确实归档进了频道（否则下面的断言是空跑）",
  inChannel === "1",
  `频道计数=${inChannel}`
);
await screenshot(s, `${SHOTS}/web-05-channel.png`);

// ---------------------------------------------------------------- 删频道
console.log("== 删频道（里面的记录不该消失）==");
const timelineCount = () =>
  evaluate(s, `document.querySelector('.nav .nav-count').innerText.trim()`);

const before = await timelineCount();
ok("删之前时间线里有 2 条", before === "2", `实际 ${before}`);

await click(s, ".nav-item.group .row-action");
await sleep(1400);

const dialog = s.dialogs.at(-1) ?? "";
ok("弹了确认框", dialog.length > 0, JSON.stringify(s.dialogs));
ok(
  "确认框承诺「记录会回到收件箱」",
  dialog.includes("回到收件箱"),
  `实际：${dialog}`
);

await waitFor(s, `!document.body.innerText.includes('浏览器建的频道')`, "频道消失");
ok("频道确实删掉了", true);

const after = await timelineCount();
ok(
  "**频道里的记录没有跟着消失**",
  after === "2",
  `删频道前 ${before} 条，删完变成 ${after} 条 —— 确认框说了会回到收件箱，就不能删掉它们`
);
ok("那条记录仍然看得到", (await text()).includes("地铁上想到的一个点子"));
await screenshot(s, `${SHOTS}/web-06-after-channel-delete.png`);

// ---------------------------------------------------------------- 会话持久化
console.log("== 刷新之后仍然登录 ==");
await s.send("Page.reload", { ignoreCache: true });
await sleep(1200);
ok(
  "刷新后直接是主界面，不用重新登录",
  await evaluate(s, "!!document.querySelector('.app')")
);
ok(
  "刷新后数据还在",
  (await text()).includes("地铁上想到的一个点子")
);

// ---------------------------------------------------------------- 收尾
console.log("== 页面健康 ==");
ok(
  "整轮下来页面零报错",
  s.consoleErrors.length === 0,
  s.consoleErrors.join(" | ")
);

console.log(failed ? `\n${failed} 条失败` : "\n全部通过");
process.exit(failed ? 1 : 0);
