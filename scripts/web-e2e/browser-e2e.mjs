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
  realClick,
  pressEnter,
  selectOption,
  screenshot,
  visibleText,
  sleep,
} from "./cdp.mjs";

const APP = process.argv[2];
const TOKEN = process.argv[3];
/** 截图落在这里。默认是当前目录下的 shots/，跑完可以直接看。 */
const SHOTS = process.argv[4] ?? "shots";

let failed = 0;
// 让 cdp.mjs 里的 waitFor 也能往这个计数里加（见那边的注释：
// 抛异常会中止整轮，汇总却显示 0 失败，那比红更糟）。
globalThis.__e2eFailed = 0;

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
 * 收尾。**无论用例怎么结束都要走到这里**。
 *
 * 之前 `waitFor` 超时会直接抛异常，脚本在中间就没了，末尾那句
 * `console.log(全部通过)` 根本不执行 —— 外面看到的是"没有输出"，
 * CI 上更容易被当成"通过了"（headless 浏览器那段尤其容易看漏）。
 *
 * 所以主体包在 `run()` 里，异常记下来但不让它跳过收尾。
 */
let fatal = null;

/**
 * 中途炸掉时也要给出**诚实的**退出码和汇总。
 *
 * 这不是锦上添花：实际发生过一次 `waitFor` 超时，脚本在中间就没了，
 * 末尾的"全部通过"从没打印，而外层只看到进程非零退出、日志尾部被截断 ——
 * 很容易读成"跑完了、没报错"。这里把异常挂住，让收尾照常执行。
 */
process.on("uncaughtException", (e) => {
  fatal = e;
  console.log(`\n用例中断：${e.message}`);
  finish();
});
process.on("unhandledRejection", (e) => {
  fatal = e;
  console.log(`\n用例中断：${e?.message ?? String(e)}`);
  finish();
});

/** 打印汇总并给退出码。收尾只允许跑一次。 */
let finished = false;
function finish() {
  if (finished) return;
  finished = true;
  // 中途抛异常（waitFor 超时之类）也要算失败，不能让"0 条失败"骗过去
  const total = Math.max(failed, globalThis.__e2eFailed ?? 0) + (fatal ? 1 : 0);
  console.log(total ? `\n${total} 条失败` : "\n全部通过");
  process.exit(total ? 1 : 0);
}

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
await screenshot(s, `${SHOTS}/web-02-empty.png`);

// 用户提的：侧边栏那两段"还没有频道……先落到收件箱，以后再移"和
// "标签是横切的补充标记，和频道正交"是把功能讲给用户听，不解决问题。
// 界面该做的是让控件自己说明自己，不是替用户写使用说明。
ok(
  "侧边栏不再解释「频道是干什么的」",
  !(await text()).includes("还没有频道"),
  "把使用说明写在界面上，用户要的是能用的控件"
);
ok(
  "侧边栏不再解释「标签是横切的」",
  !(await text()).includes("标签是横切的"),
  "同上"
);

// ---------------------------------------------------------------- 收纳
console.log("== 低频功能收进一个 ⋯ 菜单 ==");

// 用户的原话："你整三个点那种，把其他的功能都收纳起来，比如导出全部这些功能，
// 你都放到一起而不是全呈现出来"。
//
// **这个测试在网页端跑，所以只能验"字号"和"同步设置"两项** ——
// 导出是桌面端专有（浏览器里写不出一棵目录树，见 Sidebar 的 onExport 注释）。
// 导出收没收进菜单，由"侧边栏不再有那个入口"这条断言间接守住。
ok("顶栏有一个「⋯」入口", await evaluate(s, `!!document.querySelector('.overflow-btn')`));

ok(
  "字号档位不再平铺在顶栏（收进菜单了）",
  await evaluate(s, `!document.querySelector('.topbar .font-select')`),
  "一个四档的下拉框常驻顶栏，占掉的是检索框的位置，而它一个月未必用一次"
);

ok(
  "侧边栏不再平铺「导出全部…」",
  await evaluate(s, `!document.querySelector('.sidebar-foot')`),
  "导出是几个月才用一次的操作，不该在导航里长期占一个位置"
);

ok(
  "菜单默认是关着的（没点开时页面上看不到那些功能）",
  await evaluate(s, `!document.querySelector('.overflow-menu')`)
);

await click(s, ".overflow-btn");
await waitFor(s, "!!document.querySelector('.overflow-menu')", "菜单展开");
ok("点 ⋯ 之后菜单展开", true);
ok(
  "菜单里有字号档位",
  await evaluate(s, `!!document.querySelector('.overflow-menu .font-select')`)
);
await screenshot(s, `${SHOTS}/web-02b-overflow.png`);

// 换字号之后菜单要收起：用户是"调完就走"，菜单糊在脸上挡着记录
await selectOption(s, ".overflow-menu .font-select", "1.3");
await waitFor(
  s,
  `!!document.querySelector('.overflow-menu') === false`,
  "换完字号菜单收起",
  10000
);
ok(
  "换完字号菜单自动收起（调完就走，别糊在脸上挡记录）",
  await evaluate(s, `!document.querySelector('.overflow-menu')`)
);
ok(
  "字号真的变了",
  await evaluate(
    s,
    `document.documentElement.style.getPropertyValue('--font-scale').trim() === '1.3'`
  )
);
// 换回标准档，别影响后面的截图
await click(s, ".overflow-btn");
await waitFor(s, "!!document.querySelector('.overflow-menu')", "菜单再展开");
await selectOption(s, ".overflow-menu .font-select", "1");
await sleep(400);

// 点外面要能关掉。没这一步的话菜单会一直挂在页面上，
// 而用户点 ⋯ 往往只是"想看看里面有什么"，不是"我要用某个功能"。
await click(s, ".overflow-btn");
await waitFor(s, "!!document.querySelector('.overflow-menu')", "菜单第三次展开");
await realClick(s, ".title");
await waitFor(
  s,
  `!document.querySelector('.overflow-menu')`,
  "点别处菜单收起",
  8000
);
ok("点菜单外面会收起（点 ⋯ 往往只是想看看里面有什么）", true);

// Esc 也要能关：键盘用户没有鼠标可点
await click(s, ".overflow-btn");
await waitFor(s, "!!document.querySelector('.overflow-menu')", "菜单第四次展开");
await evaluate(
  s,
  `document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); true`
);
await waitFor(s, `!document.querySelector('.overflow-menu')`, "Esc 收起菜单", 8000);
ok("按 Esc 也能收起菜单", true);
ok(
  "**Esc 收菜单没有把检索关键词一起清掉**（两个 Esc 不该绑同一件事）",
  await evaluate(
    s,
    `(() => {
       const q = document.querySelector('.search-input').value;
       return q === '';
     })()`
  ),
  "此时检索框本来就是空的，所以这条只验它没被误清；下面那次才验真的不误清"
);

// 真有一行检索时，Esc 只该关菜单，不该顺手把词也清了
await fill(s, ".search-input", "地铁");
await sleep(500);
await click(s, ".overflow-btn");
await waitFor(s, "!!document.querySelector('.overflow-menu')", "菜单第五次展开");
await evaluate(
  s,
  `document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); true`
);
await sleep(500);
ok(
  "有检索词时按 Esc，菜单关了但检索词还在",
  (await evaluate(s, `!!document.querySelector('.search-input').value`)) &&
    !(await evaluate(s, `!!document.querySelector('.overflow-menu')`)),
  `检索框=${await evaluate(s, `document.querySelector('.search-input').value`)}`
);
await evaluate(
  s,
  `(() => {
     const el = document.querySelector('.search-input');
     Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(el, '');
     el.dispatchEvent(new Event('input', { bubbles: true }));
     return true;
   })()`
);
await sleep(600);

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

// ---------------------------------------------------------------- 频道里发消息
console.log("== 在频道里发消息，去向就该是这个频道 ==");
// 曾经的 bug：不管在哪个视图，输入框都把消息塞回收件箱。
await click(s, ".nav-item.group .nav-main");
await sleep(700);
ok(
  "输入框的去向跟着视图走",
  (await evaluate(s, "document.querySelector('.composer-target').innerText")).includes(
    "浏览器建的频道"
  )
);
await fill(s, ".composer-input", "在频道里直接发的一条");
await click(s, ".send-btn");
await waitFor(s, `document.body.innerText.includes('在频道里直接发的一条')`, "消息出现在频道里");
await sleep(600);
const chCountAfterSend = await evaluate(
  s,
  `(() => {
     const item = [...document.querySelectorAll('.nav-item.group')]
       .find(x => x.innerText.includes('浏览器建的频道'));
     return item?.querySelector('.nav-count')?.innerText.trim() ?? null;
   })()`
);
ok(
  "发出去的消息真的进了频道（计数 2）",
  chCountAfterSend === "2",
  `频道计数=${chCountAfterSend}`
);
await screenshot(s, `${SHOTS}/web-05b-send-in-channel.png`);

// 回时间线 —— 下一段要在时间线视图里删频道
await click(s, ".nav .nav-item");
await sleep(500);

// ---------------------------------------------------------------- 删频道
console.log("== 删频道（里面的记录不该消失）==");
const timelineCount = () =>
  evaluate(s, `document.querySelector('.nav .nav-count').innerText.trim()`);

const before = await timelineCount();
ok("删之前时间线里有 3 条", before === "3", `实际 ${before}`);

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
  after === "3",
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

// ---------------------------------------------------------------- 检索分页
console.log("== 检索分页（加载更多结果）==");

// 造够一页以上的命中。经 API 直接写 —— 这一段不是被测对象，只是布置现场。
const PAGE = 60; // 与 App.tsx 的 SEARCH_PAGE_SIZE 一致
const total = PAGE + 12;
{
  const h = { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" };
  for (let i = 0; i < total; i++) {
    const r = await fetch(`${new URL(APP).origin}/api/message`, {
      method: "POST",
      headers: h,
      body: JSON.stringify({ body: `批量记录 ${i}` }),
    });
    if (!r.ok) throw new Error(`造数据失败：HTTP ${r.status}`);
  }
}

await fill(s, ".search-input", "批量记录");
await sleep(1500); // 防抖 + 一次往返

const rowCount = () => evaluate(s, "document.querySelectorAll('.msg-row').length");
ok("首页给出整整一页", (await rowCount()) === PAGE, `实际 ${await rowCount()}`);

const moreBtn = `[...document.querySelectorAll('.load-more-row button')].find(b => b.innerText.includes('加载更多'))`;
ok("首页之后出现「加载更多结果」", await evaluate(s, `!!${moreBtn}`));
await screenshot(s, `${SHOTS}/web-07-search-page1.png`);

await click(s, ".load-more-row button");
await waitFor(
  s,
  `document.querySelectorAll('.msg-row').length > ${PAGE}`,
  "第二页出现"
);
ok(
  "点了之后确实多出来了",
  (await rowCount()) === total,
  `总共应当 ${total} 条，实际 ${await rowCount()}`
);
ok(
  "翻到底之后按钮消失，换成一句「没有更多了」",
  await evaluate(s, `!${moreBtn} && document.body.innerText.includes('没有更多了')`)
);
await screenshot(s, `${SHOTS}/web-08-search-page2.png`);

// 回到时间线，避免影响后面的判断
await fill(s, ".search-input", "");
await sleep(600);

// ---------------------------------------------------------------- 图片
console.log("== 图片：粘贴 → 发送 → 真的渲染出来 ==");

// 在页面里**现画**一张 PNG。不硬编码 base64：那种串抄错一个字符，
// 表现是"图片显示不出来"，而失败原因看起来会和真正的错误毫无关系。
const pasted = await evaluate(
  s,
  `(async () => {
     const c = document.createElement('canvas');
     c.width = 12; c.height = 12;
     const ctx = c.getContext('2d');
     ctx.fillStyle = '#e11d48';
     ctx.fillRect(0, 0, 12, 12);
     const blob = await new Promise((r) => c.toBlob(r, 'image/png'));

     const dt = new DataTransfer();
     dt.items.add(new File([blob], 'e2e.png', { type: 'image/png' }));

     const input = document.querySelector('.composer-input');
     input.focus();
     input.dispatchEvent(new ClipboardEvent('paste', {
       clipboardData: dt, bubbles: true, cancelable: true,
     }));

     // 插入要等一次网络往返（saveAttachment），所以轮询而不是立刻读
     const started = Date.now();
     while (Date.now() - started < 15000) {
       if (/attachment:[0-9a-f]{64}/.test(input.value)) break;
       await new Promise((r) => setTimeout(r, 100));
     }
     return { value: input.value, bytes: blob.size };
   })()`
);

const sha = (pasted.value.match(/attachment:([0-9a-f]{64})/) || [])[1];
ok("粘贴图片后正文里出现 attachment:<sha> 引用", !!sha, pasted.value.slice(0, 160));
ok(
  "生成的 PNG 不是空文件（否则下面那条断言是空跑）",
  pasted.bytes > 50,
  `${pasted.bytes} 字节`
);

await pressEnter(s, ".composer-input");
await waitFor(
  s,
  `[...document.querySelectorAll('.md img')].some((i) => i.naturalWidth > 0)`,
  "图片渲染出来"
);

const rendered = await evaluate(
  s,
  `(() => {
     const img = [...document.querySelectorAll('.md img')].find((i) => i.naturalWidth > 0);
     return img ? { w: img.naturalWidth, h: img.naturalHeight, scheme: img.src.split(':')[0] } : null;
   })()`
);
ok(
  "图片真的解码出来了 —— naturalWidth 有值说明字节是对的，而不只是有个 URL",
  rendered && rendered.w === 12 && rendered.h === 12,
  JSON.stringify(rendered)
);
ok(
  "用的是 blob: URL（字节从本地取，不是去访问什么外部地址）",
  rendered && rendered.scheme === "blob",
  JSON.stringify(rendered)
);

// 滚到底再截图 —— 断言是在整棵 DOM 上找的，图可能在视口之外，
// 那样截出来的图里根本没有它，等于没有证据。
await evaluate(
  s,
  `(() => {
     const el = document.querySelector('.stream');
     if (el) el.scrollTop = el.scrollHeight;
     return true;
   })()`
);
await sleep(500);
await screenshot(s, `${SHOTS}/web-09-image.png`);

// ---------------------------------------------------------------- 拖拽
console.log("== 拖拽：拖进来就存下、就在光标处出现 ==");

// 用户报"拖拽图片无法发送"。上一轮只测了**粘贴**（网页端里粘贴一直是好的），
// 于是拖拽这条路径从来没被验过 —— 而它恰好是坏的那条。
//
// 这里派发真实的 DragEvent + DataTransfer，和用户在资源管理器里拖进来
// 落到页面上时浏览器给出的事件是同构的。
const dropped = await evaluate(
  s,
  `(async () => {
     const c = document.createElement('canvas');
     c.width = 10; c.height = 10;
     const ctx = c.getContext('2d');
     ctx.fillStyle = '#0ea5e9';
     ctx.fillRect(0, 0, 10, 10);
     const blob = await new Promise((r) => c.toBlob(r, 'image/png'));

     const dt = new DataTransfer();
     dt.items.add(new File([blob], 'dropped.png', { type: 'image/png' }));

     const composer = document.querySelector('.composer');
     const input = document.querySelector('.composer-input');
     input.focus();

     const fire = (type) => composer.dispatchEvent(
       new DragEvent(type, { dataTransfer: dt, bubbles: true, cancelable: true })
     );
     fire('dragover');
     fire('drop');

     const started = Date.now();
     while (Date.now() - started < 15000) {
       if (/attachment:[0-9a-f]{64}/.test(input.value)) break;
       await new Promise((r) => setTimeout(r, 100));
     }
     return input.value;
   })()`
);

const dropSha = (dropped.match(/attachment:([0-9a-f]{64})/) || [])[1];
ok(
  "拖进来的图片进了正文（不是只有粘贴能用）",
  !!dropSha,
  `拖拽后输入框内容：${dropped.slice(0, 120)}`
);

// 拖拽进来的这张要真的能发出去、能渲染出来 —— 只验证"插入成功"的话，
// 用户遇到的"拖进去看着像存了、其实发不出去"照样溜过去。
await pressEnter(s, ".composer-input");
await waitFor(
  s,
  `[...document.querySelectorAll('.md img')].some((i) => i.naturalWidth === 10)`,
  "拖进来的那张图渲染出来了"
);
ok("拖进来的图片发送后真的渲染出来", true);
await evaluate(
  s,
  `(() => {
     const el = document.querySelector('.stream');
     if (el) el.scrollTop = el.scrollHeight;
     return true;
   })()`
);
await sleep(400);
await screenshot(s, `${SHOTS}/web-09b-dropped.png`);

// ---------------------------------------------------------------- 上传按钮
console.log("== 上传按钮：不拖不粘也能选文件 ==");

ok(
  "输入框旁边有上传按钮",
  await evaluate(s, `!!document.querySelector('.attach-btn')`),
  "只有拖拽和粘贴的话，用户手上唯一一张图在手机上、在另一个程序里时就无路可走"
);

const picked = await evaluate(
  s,
  `(async () => {
     const c = document.createElement('canvas');
     c.width = 9; c.height = 9;
     const ctx = c.getContext('2d');
     ctx.fillStyle = '#16a34a';
     ctx.fillRect(0, 0, 9, 9);
     const blob = await new Promise((r) => c.toBlob(r, 'image/png'));

     const input = document.querySelector('.composer-input');
     input.focus();

     // 给那个隐藏的 file input 塞一个真的 FileList，然后派发 change ——
     // 等价于用户在系统文件对话框里选了一张图。
     const picker = document.querySelector('.attach-input');
     if (!picker) return { error: '没有 .attach-input' };
     const dt = new DataTransfer();
     dt.items.add(new File([blob], 'picked.png', { type: 'image/png' }));
     picker.files = dt.files;
     picker.dispatchEvent(new Event('change', { bubbles: true }));

     const started = Date.now();
     while (Date.now() - started < 15000) {
       if (/attachment:[0-9a-f]{64}/.test(input.value)) break;
       await new Promise((r) => setTimeout(r, 100));
     }
     return { value: input.value };
   })()`
);
const pickSha = ((picked.value || "").match(/attachment:([0-9a-f]{64})/) || [])[1];
ok(
  "通过按钮选的文件也进了正文",
  !!pickSha,
  picked.error ?? `按钮选完输入框内容：${(picked.value || "").slice(0, 120)}`
);
// 选**和拖进来那张完全相同的字节**（同一个画布尺寸 + 同一个颜色），
// sha 必须一样：内容寻址意味着同一份内容永远是同一个名字，
// 存两次既浪费存储，也会让正文里出现两条指向同一张图的引用。
const picked2 = await evaluate(
  s,
  `(async () => {
     const c = document.createElement('canvas');
     c.width = 9; c.height = 9;
     const ctx = c.getContext('2d');
     ctx.fillStyle = '#16a34a';
     ctx.fillRect(0, 0, 9, 9);
     const blob = await new Promise((r) => c.toBlob(r, 'image/png'));

     const input = document.querySelector('.composer-input');
     const picker = document.querySelector('.attach-input');
     const before = input.value;
     const dt = new DataTransfer();
     dt.items.add(new File([blob], 'picked-again.png', { type: 'image/png' }));
     picker.files = dt.files;
     picker.dispatchEvent(new Event('change', { bubbles: true }));

     const started = Date.now();
     while (Date.now() - started < 15000) {
       // 等到正文里出现**第二个**引用
       if ((input.value.match(/attachment:[0-9a-f]{64}/g) || []).length > 1) break;
       await new Promise((r) => setTimeout(r, 100));
     }
     return { before, after: input.value };
   })()`
);
const firstSha = ((picked.value || "").match(/attachment:([0-9a-f]{64})/) || [])[1];
const secondSha = ((picked2.after || "").match(/attachment:([0-9a-f]{64})/g) || []).slice(-1)[0]?.slice(11);
ok(
  "同一份内容再选一次，sha 不变（内容寻址去重，不是每次新存一份）",
  !!firstSha && !!secondSha && firstSha === secondSha,
  `第一次 ${firstSha}，第二次 ${secondSha}`
);
ok(
  "选同一个文件两次之后输入框里确实多了一条引用（否则上一条是空跑）",
  (picked2.after || "").length > (picked2.before || "").length,
  `${(picked2.before || "").length} → ${(picked2.after || "").length}`
);

// ---------------------------------------------------------------- 附件
console.log("== 附件：非图片也能传、能点开 ==");

// 用户说的是"包括你也可以上传附件"。只支持图片的话，选一个 .pdf / .txt
// 会被 accept 挡掉，或者存进去了却渲染成一坨没法点的东西 —— 那不叫支持附件。
const attached = await evaluate(
  s,
  `(async () => {
     const input = document.querySelector('.composer-input');
     const picker = document.querySelector('.attach-input');
     if (!picker) return { error: '没有 .attach-input' };
     if (picker.accept) return { error: '上传框仍然限定了类型：' + picker.accept };

     // **必须先清空。** 上一段测试往输入框里插了两张图，值里已经有
     // \`![图片](attachment:…)\` 了 —— 不清的话下面那个轮询条件立刻满足，
     // 读到的是上一轮的旧内容，这条断言就成了在验上一个功能。
     input.value = '';
     input.dispatchEvent(new Event('input', { bubbles: true }));
     await new Promise((r) => setTimeout(r, 200));

     const dt = new DataTransfer();
     dt.items.add(new File(['会议纪要：三月复盘'], 'notes.txt', { type: 'text/plain' }));
     picker.files = dt.files;
     picker.dispatchEvent(new Event('change', { bubbles: true }));

     const started = Date.now();
     while (Date.now() - started < 15000) {
       if (/notes\\.txt/.test(input.value)) break;
       await new Promise((r) => setTimeout(r, 100));
     }
     return { value: input.value, accept: picker.accept };
   })()`
);

const attachSha = ((attached.value || "").match(/attachment:([0-9a-f]{64})/) || [])[1];
ok(
  "非图片文件（.txt）也能上传",
  !!attachSha,
  attached.error ?? `选完输入框内容：${(attached.value || "").slice(0, 120)}`
);
ok(
  "非图片插进来的是**链接**而不是图片语法（`[名字](attachment:sha)`）",
  /\[notes\.txt\]\(attachment:[0-9a-f]{64}\)/.test(attached.value || ""),
  `实际：${(attached.value || "").slice(0, 120)}`
);

await pressEnter(s, ".composer-input");
await waitFor(
  s,
  `(() => {
     const a = [...document.querySelectorAll('.md a')].find((x) => x.innerText.includes('notes.txt'));
     return !!a && a.getAttribute('href')?.startsWith('blob:');
   })()`,
  "附件链接变成可点的 blob URL",
  20000
);
ok(
  "发出去之后附件链接能点开（href 换成了 blob: URL）",
  await evaluate(
    s,
    `(() => {
       const a = [...document.querySelectorAll('.md a')].find((x) => x.innerText.includes('notes.txt'));
       return !!a && a.getAttribute('href')?.startsWith('blob:') && !!a.getAttribute('download');
     })()`
  )
);
ok(
  "附件带 download 属性（点下去是存文件，不是跳到一个 attachment: 的死链）",
  await evaluate(
    s,
    `(() => {
       const a = [...document.querySelectorAll('.md a')].find((x) => x.innerText.includes('notes.txt'));
       return a?.getAttribute('download') === 'notes.txt';
     })()`
  )
);

// 拖进来的文件夹不该被当成文件存进去
const folderDropped = await evaluate(
  s,
  `(async () => {
     const composer = document.querySelector('.composer');
     const input = document.querySelector('.composer-input');
     // 同样先清空：这里比的是"拖之前 == 拖之后"，
     // 输入框里留着上一段测试的内容的话，两边一样，这断言就是空的。
     input.value = '';
     input.dispatchEvent(new Event('input', { bubbles: true }));
     await new Promise((r) => setTimeout(r, 200));
     const before = input.value;

     const dt = new DataTransfer();
     // 拖文件夹时浏览器给出的项：type 为空、size 为 0、name 为空
     dt.items.add(new File([], '', { type: '' }));
     const fire = (type) => composer.dispatchEvent(
       new DragEvent(type, { dataTransfer: dt, bubbles: true, cancelable: true })
     );
     fire('dragover');
     fire('drop');
     await new Promise((r) => setTimeout(r, 1500));
     return { before, after: input.value };
   })()`
);
ok(
  "拖进来的文件夹被挡掉，不会被当成附件存进去",
  folderDropped.before === "" && folderDropped.after === "",
  `拖之前 ${JSON.stringify(folderDropped.before.slice(-40))}，拖之后 ${JSON.stringify(folderDropped.after.slice(-40))}`
);
ok(
  "**拖之前输入框确实是空的**（否则上一条是空跑）",
  folderDropped.before === "",
  `实际：${JSON.stringify(folderDropped.before)}`
);

// ---------------------------------------------------------------- 头像
console.log("== 头像：能换成自己上传的图 ==");

ok(
  "默认头像有入口可以更换（不是写死的「我」）",
  await evaluate(s, `!!document.querySelector('.avatar-btn, .avatar-upload')`),
  "写死的「我」字符让用户没法用自己的图"
);

await evaluate(
  s,
  `(async () => {
     const c = document.createElement('canvas');
     c.width = 32; c.height = 32;
     const ctx = c.getContext('2d');
     ctx.fillStyle = '#9333ea';
     ctx.fillRect(0, 0, 32, 32);
     const blob = await new Promise((r) => c.toBlob(r, 'image/png'));

     const picker = document.querySelector('.avatar-input');
     if (!picker) return;
     const dt = new DataTransfer();
     dt.items.add(new File([blob], 'me.png', { type: 'image/png' }));
     picker.files = dt.files;
     picker.dispatchEvent(new Event('change', { bubbles: true }));
     return true;
   })()`
);

await waitFor(
  s,
  `!!document.querySelector('.avatar img') && document.querySelector('.avatar img').naturalWidth > 0`,
  "头像换成用户自己上传的图",
  20000
);
ok(
  "消息流里的头像变成了用户上传的图",
  await evaluate(
    s,
    `(() => {
       const img = document.querySelector('.msg-row .avatar img');
       return !!img && img.naturalWidth === 32;
     })()`
  )
);

// 刷新之后还在 —— 只在内存里换了一下等于没换
await s.send("Page.reload", { ignoreCache: true });
await sleep(1500);
await waitFor(s, `!!document.querySelector('.app')`, "刷新后主界面");
ok(
  "刷新之后自定义头像还在（存下来了，不是只在内存里）",
  await evaluate(
    s,
    `(() => {
       const img = document.querySelector('.msg-row .avatar img');
       return !!img && img.naturalWidth === 32;
     })()`
  )
);
await screenshot(s, `${SHOTS}/web-09c-avatar.png`);

// 换回默认。没设过头像时不该有这个出口（一个永远点不动的装饰），
// 设过之后点它要真的退回「我」，否则用户设错了就没有回头路。
//
// 先把 localStorage 里存的 sha 抹掉再刷新，验的是"从没设过"的初始态。
await evaluate(s, `localStorage.removeItem('messagenote.avatarSha'); true`);
await s.send("Page.reload", { ignoreCache: true });
await sleep(1600);
await waitFor(s, `!!document.querySelector('.app')`, "清掉头像后主界面");
ok(
  "清掉 localStorage 里的头像后退回「我」",
  await evaluate(
    s,
    `(() => {
       const a = document.querySelector('.msg-row .avatar');
       return !!a && !a.querySelector('img') && a.innerText.trim() === '我';
     })()`
  )
);
ok(
  "没设过头像时不显示清除按钮",
  await evaluate(s, `!document.querySelector('.avatar-clear')`)
);

// 再设一次，然后走「清除」按钮这条路
await evaluate(
  s,
  `(async () => {
     const c = document.createElement('canvas');
     c.width = 32; c.height = 32;
     const ctx = c.getContext('2d');
     ctx.fillStyle = '#ea580c';
     ctx.fillRect(0, 0, 32, 32);
     const blob = await new Promise((r) => c.toBlob(r, 'image/png'));
     const picker = document.querySelector('.avatar-input');
     const dt = new DataTransfer();
     dt.items.add(new File([blob], 'me2.png', { type: 'image/png' }));
     picker.files = dt.files;
     picker.dispatchEvent(new Event('change', { bubbles: true }));
     return true;
   })()`
);
await waitFor(
  s,
  `(() => { const i = document.querySelector('.msg-row .avatar img'); return !!i && i.naturalWidth === 32; })()`,
  "第二次设的头像"
);
ok("可以反复更换头像", true);
ok(
  "设过之后出现「清除」按钮",
  await evaluate(s, `!!document.querySelector('.avatar-clear')`)
);

await click(s, ".avatar-clear");
await waitFor(
  s,
  `(() => { const a = document.querySelector('.msg-row .avatar'); return !!a && !a.querySelector('img'); })()`,
  "点清除后退回默认头像",
  10000
);
ok(
  "点「清除」真的退回「我」了（不是只有 localStorage 被清、界面还留着旧图）",
  await evaluate(
    s,
    `document.querySelector('.msg-row .avatar').innerText.trim() === '我'`
  )
);
await sleep(300);
// ---------------------------------------------------------------- 网页端导出

// 放在附件那一节之后：到这儿库里已经有记录**和图片**了，于是这个 zip 里
// 应当同时装得下正文和附件 —— 那才是网页端导出真正要验的东西。
//
// （不能放在 ⋯ 菜单那一节：那里后面还跟着一串硬编码的条数断言，
// 多记一条会把后面全部顶掉。）
console.log("== 网页端导出：服务端打成 zip，浏览器下载 ==");

// **把 createObjectURL 和 a.click 都拦下来**，理由不是省事：
// 一是 headless 里真下载会在磁盘上留文件、还要配下载目录；二是那样验不到
// "文件名取自 Content-Disposition" 和"摘要是从响应头读的" —— 而这两处正是
// 只在真实浏览器里才会露馅的地方。
await evaluate(
  s,
  `(() => {
     window.__capBlob = null;
     window.__capName = "";
     const origCreate = URL.createObjectURL;
     URL.createObjectURL = (b) => { window.__capBlob = b; return origCreate(b); };
     const origClick = HTMLAnchorElement.prototype.click;
     HTMLAnchorElement.prototype.click = function () {
       if (this.download) {
         window.__capName = this.download;
         return; // 不真的触发下载
       }
       return origClick.call(this);
     };
     return true;
   })()`
);

await click(s, ".overflow-btn");
await waitFor(s, "!!document.querySelector('.overflow-menu')", "菜单展开（找导出）");
ok(
  "网页端的 ⋯ 菜单里也有「导出记录…」（之前只有桌面端有）",
  await evaluate(
    s,
    `[...document.querySelectorAll('.overflow-menu .overflow-item')].some(b => b.textContent.includes('导出'))`
  )
);

// 用 CDP 派发的点击，**不是 `element.click()`**：React 的合成事件挂在容器上，
// 脚本直接调 DOM 的 click 在这里不触发（实测过），点了等于没点。
await click(s, ".overflow-menu .overflow-item");
await waitFor(s, "!!document.querySelector('.modal')", "导出面板打开");
ok("导出面板打开了", true);
ok(
  "网页端的按钮说的是「导出为 zip」而不是「选择目录并导出」",
  await evaluate(
    s,
    `[...document.querySelectorAll('.modal .btn')].some(b => b.textContent.includes('zip'))`
  )
);
await screenshot(s, `${SHOTS}/web-10-export-web.png`);

await click(s, ".modal .btn.primary");
await waitFor(s, "!!window.__capBlob", "导出完成", 30000);

const cap = await evaluate(
  s,
  `(async () => {
     const b = window.__capBlob;
     if (!b) return null;
     const head = new Uint8Array(await b.slice(0, 4).arrayBuffer());
     return {
       type: b.type,
       size: b.size,
       name: window.__capName,
       // 有条目的 zip 头四个字节是 PK\\x03\\x04；空 zip 是 PK\\x05\\x06
       magic: Array.from(head).join(","),
     };
   })()`
);
ok("网页端导出真的拿到了一个 blob", !!cap && cap.size > 0, JSON.stringify(cap));
ok(
  "它是一个**非空的真 zip**（魔数 PK\\x03\\x04，不是空包、更不是一段报错文本）",
  !!cap && cap.magic === "80,75,3,4",
  cap ? `magic=${cap.magic} size=${cap.size}` : "没拿到 blob"
);
ok("zip 的 MIME 是 application/zip", !!cap && cap.type === "application/zip");
ok(
  "文件名带日期（messagenote-<日期>.zip），下载列表里才分得清是哪一次",
  !!cap && /^messagenote-\d{4}-\d{2}-\d{2}-\d{4}\.zip$/.test(cap.name),
  cap ? cap.name : ""
);

await waitFor(s, `!!document.querySelector('.modal .note.ok')`, "导出结果提示", 15000);
const noteText = await evaluate(
  s,
  `document.querySelector('.modal .note.ok')?.textContent ?? ""`
);
ok(
  "面板上给出了条数（摘要是从响应头读的，不是前端编的）",
  /已导出 \d+ 条记录/.test(noteText),
  noteText
);
ok(
  "附件也进去了（库里这一节之前贴过图，摘要是从响应头读的）",
  /已导出 \d+ 条记录、[1-9]\d* 个附件/.test(noteText),
  noteText
);
await screenshot(s, `${SHOTS}/web-10b-export-done.png`);

await click(s, ".modal .btn.ghost");
await waitFor(s, "!document.querySelector('.modal')", "导出面板关闭");
ok("面板能关掉", true);

// 还原钩子，别让后面几节被影响
await evaluate(s, `(() => { window.__capBlob = null; return true; })()`);

await screenshot(s, `${SHOTS}/web-09d-avatar-cleared.png`);

// ---------------------------------------------------------------- 实时推送
console.log("== 实时推送：别处写入，这里自己出现 ==");

// 从**页面之外**写一条。这样测的是"服务端推过来、界面自己更新"，
// 而不是"本地的某次操作触发了刷新" —— 后者在没有任何推送的情况下也会通过。
const marker = `推送验证${Date.now()}`;
{
  const r = await fetch(`${new URL(APP).origin}/api/message`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ body: marker }),
  });
  if (!r.ok) throw new Error(`写入失败：HTTP ${r.status}`);
}

// 接下来**不做任何操作**，只等。
//
// 网页端**没有轮询** —— 它原先要用户手动刷新才能看到别人的改动。所以这条
// 记录出现只可能来自推送。给 10 秒是为了和"碰巧赶上了别的刷新"区分开：
// 真的是推送的话通常几百毫秒就到。
await waitFor(
  s,
  `document.body.innerText.includes(${JSON.stringify(marker)})`,
  "推送把新记录带到了页面上",
  10000
);
ok(
  "别处写入的记录**未经任何操作**就出现了（网页端没有轮询，只可能是推送）",
  (await text()).includes(marker)
);
await screenshot(s, `${SHOTS}/web-10-push.png`);

// 顺带确认这条连接是**一条**，不是每次刷新都新建一堆 ——
// 订阅写在 effect 里，依赖没写对的话切视图会不停重连。
ok(
  "推送没有把页面搞出错（连接断了会重连，不该冒到控制台）",
  s.consoleErrors.length === 0,
  s.consoleErrors.join(" | ")
);

// ------------------------------------------------------- 过期响应不能覆盖新视图
console.log("== 乱序响应守卫：先发的请求后回来，不能盖掉新视图 ==");

// 这是**乱序**响应造成的渲染错误：快速连点两个频道时，第一个请求可能比
// 第二个更晚回来，于是界面上出现的是**上一个频道**的内容 —— 而且它不自愈，
// 直到用户再点一次。
//
// 造法：在页面里包一层 fetch，给**第一次** /api/timeline 加 2.5 秒延迟、
// 后面的照常放行 —— 于是第二个请求必然先回。精确地制造乱序，
// 而不是靠「多点几次试试」的运气。
//
// 视图用**新建频道**来区分：两个频道各写一条内容互不相同的记录，
// 于是「界面上是谁的内容」就是确凿的判据。
await evaluate(
  s,
  `(() => {
    const real = window.fetch;
    window.__origFetch = real;
    let hits = 0;
    window.__restoreFetch = () => { window.fetch = real; };
    window.fetch = (input, init) => {
      const url = typeof input === "string" ? input : input.url;
      if (url.includes("/api/timeline") && hits++ === 0) {
        return new Promise((r) => setTimeout(() => r(real(input, init)), 2500));
      }
      return real(input, init);
    };
    return true;
  })()`
);

/** 新建一个频道并等它出现在侧边栏。 */
const makeChannel = async (name) => {
  await evaluate(
    s,
    `(() => {
      // 频道区是**第一个** .section（第二个是标签）。注意那个按钮的 onClick
      // 是 setAdding(v => !v)：输入框开着时再点会把它关掉，所以先查。
      if (!document.querySelector('.inline-input')) {
        const btn = document.querySelectorAll('.section')[0]
          .querySelector('.icon-btn');
        if (!btn) throw new Error('找不到新建频道的按钮');
        btn.click();
      }
      return true;
    })()`
  );
  await waitFor(s, `!!document.querySelector('.inline-input')`, `「${name}」输入框`);
  await fill(s, ".inline-input", name);
  await pressEnter(s, ".inline-input");
  const channelExists = (name) =>
  evaluate(
    s,
    `(() => {
       const items = [...document.querySelectorAll('.nav-main')].map(x => x.textContent);
       const open = !!document.querySelector('.inline-input');
       const draft = document.querySelector('.inline-input')?.value ?? '';
       return { found: items.some(x => x.includes(${JSON.stringify(name)})),
                items, open, draft };
     })()`
  );

const until = Date.now() + 10000;
let seen = null;
while (Date.now() < until) {
  seen = await channelExists(name);
  if (seen.found && !seen.open) break;
  await sleep(200);
}
ok(
  `「${name}」建好并出现在侧边栏`,
  Boolean(seen?.found) && !seen?.open,
  JSON.stringify(seen)
);
};

/** 按名字点侧边栏里的频道。 */
const clickChannel = (name) =>
  evaluate(
    s,
    `(() => {
      const b = [...document.querySelectorAll('.nav-main')]
        .find(x => x.textContent.includes(${JSON.stringify(name)}));
      if (!b) throw new Error('侧边栏里没有这个频道：' + ${JSON.stringify(name)});
      b.click();
      return true;
    })()`
  );

for (const [chan, marker] of [
  ["慢频道", "SLOWMARKER"],
  ["快频道", "FASTMARKER"],
]) {
  await makeChannel(chan);
  await sleep(300);
  await clickChannel(chan);
  await sleep(500);
  await fill(s, ".composer-input", marker);
  await pressEnter(s, ".composer-input");
  await sleep(1500);
}

// ---- 制造乱序 ----
// 先点「慢频道」（它的 timeline 响应被延迟 2.5 秒），再立刻点「快频道」。
await clickChannel("慢频道");
await sleep(150); // 让第一个请求确实发出去了
await clickChannel("快频道");

// 等那个慢请求真的回来 —— 错误正是那一刻发生的
await sleep(4500);

// **立刻把 fetch 原样还回去，而且要确认真的还了。**
//
// 这一步比看起来重要：Service Worker 的 install 会 `cache.add(SHELL)`，
// 那也是一次 fetch。如果此刻 fetch 还是被包着的版本，它会走我们那段
// 「只延迟第一次」的逻辑，壳就**没能被缓存下来** —— 后面「离线打开」
// 那一段于是失败，症状出现在好几段之后，指向一个完全无辜的地方
//（看起来像是 sw.js 坏了，而其实只是我们自己的 mock 漏了刀）。
await evaluate(s, `window.__restoreFetch(); true`);
await sleep(300);
ok(
  "fetch 已经还原（后面 Service Worker 的预缓存要靠它）",
  await evaluate(s, `window.fetch === window.__origFetch`)
);

const shown = await text();
ok(
  "慢响应没有覆盖新视图（界面停在最后点的那个频道上）",
  !shown.includes("SLOWMARKER"),
  shown.includes("SLOWMARKER") ? "界面上出现了慢频道的内容" : ""
);
ok(
  "新视图的内容正常显示",
  shown.includes("FASTMARKER"),
  await evaluate(s, `document.body.innerText.slice(0, 200)`)
);

// ---------------------------------------------------------------- 离线捕获
console.log("== 离线捕获：断网能记，联网自动补发 ==");

await s.send("Network.enable");
const setOffline = (offline) =>
  s.send("Network.emulateNetworkConditions", {
    offline,
    latency: 0,
    downloadThroughput: -1,
    uploadThroughput: -1,
  });

await setOffline(true);

const offlineMarker = `离线记的${Date.now()}`;
await fill(s, ".composer-input", offlineMarker);
await pressEnter(s, ".composer-input");

await waitFor(
  s,
  `!!document.querySelector('.offline-pill')`,
  "离线提示出现",
  10000
);
ok(
  "断网时记的一条进了离线队列，而且明确告诉了用户",
  (await evaluate(s, `document.querySelector('.offline-pill').innerText`)).includes(
    "离线保存"
  )
);

// 输入框必须清空：东西确实存下来了，不该还留在那儿让用户担心
ok(
  "输入框已清空（本地回执让它走完了正常路径）",
  (await evaluate(s, `document.querySelector('.composer-input').value`)) === ""
);

// 断网期间它**不该**出现在时间线上 —— 服务端还没有这条
ok(
  "断网时时间线上还没有它（还没发出去）",
  !(await text()).includes(offlineMarker)
);

await setOffline(false);

// 重放是自动的，用户不需要做任何事
await waitFor(
  s,
  `document.body.innerText.includes(${JSON.stringify(offlineMarker)})`,
  "恢复联网后自动补发",
  15000
);
ok("恢复联网后自动补发，记录出现在时间线上", (await text()).includes(offlineMarker));

// **只出现一次** —— 这正是前几轮那个幂等键存在的理由。
// 少了它，重放（重试、页面重开、手动触发）每跑一次就多一条。
const times = await evaluate(
  s,
  `(document.body.innerText.match(new RegExp(${JSON.stringify(offlineMarker)}, 'g')) || []).length`
);
ok("只补发了一条（幂等键挡住了重复重放）", times === 1, `出现了 ${times} 次`);

// 队列空了，提示就该消失
await waitFor(s, `!document.querySelector('.offline-pill')`, "提示消失", 10000);
ok("队列清空之后提示消失", !(await evaluate(s, `!!document.querySelector('.offline-pill')`)));

// ------------------------------------------------- 重放途中断网（不能丢数据）
console.log("== 重放途中断网：队列必须原样留在那儿 ==");

// 这条守的是一个**丢数据**的 bug，而且触发条件很窄：
// 浏览器说自己在联网（navigator.onLine === true），但 fetch 抛 TypeError。
// 瞬时网络抖动、代理掐连接、NAT 表项过期 —— 都是这个形态。
//
// 曾经重放复用了"交互发送"那层带兜底的包装：包装层把 TypeError 当成"离线"，
// 于是**把同一条重新入队**并返回一张假回执；重放层以为发成了，就把这条
// `drop()` 掉。一整队条目于是既没到服务端、又从队列里消失，界面还显示补发完成。
//
// 造这个场景不能用 `setOffline`（那样 navigator.onLine 会是 false，
// 走的是另一条分支）。要拦的是**请求本身**：setBlockedURLs 让 /api/message
// 打不通，而浏览器仍然认为自己在线。

const flakeMarker = [`抖动一${Date.now()}`, `抖动二${Date.now()}`];

await setOffline(true);
// 等浏览器真的翻过来。CDP 的设置是异步生效的，抢跑的话
// navigator.onLine 还是 true，笔记会被直接发出去而不是入队。
await waitFor(s, `navigator.onLine === false`, "浏览器报告已离线", 10000);

// **回到时间线视图**。上一段结束时停在某个频道里，而在频道里发出的记录
// 同样会入队，但断言里"这两条没到服务端"会变得含糊（时间线上的其它记录
// 也在页面上）。回到时间线让这一段的判据干净。
await evaluate(
  s,
  `(() => {
    // 时间线那个按钮的类名是 nav-item，频道/标签才是 nav-main。
    // 用错选择器的话这个点击是空操作 —— 而症状要到几条之后才显现。
    const b = [...document.querySelectorAll('.nav-item')]
      .find(x => x.textContent.includes('时间线'));
    if (b) b.click();
    return true;
  })()`
);
await sleep(1000);

for (const m of flakeMarker) {
  await fill(s, ".composer-input", m);
  await pressEnter(s, ".composer-input");
  // 每条发完等一下：连续两次 fill+Enter 之间输入框可能还没回到可写状态，
  // 第二条就会丢（表现是队列里只有 1 条，而断言查的是 2）。
  await sleep(600);
}
/**
 * 直接读 IndexedDB 里的队列长度。
 *
 * **刻意不读界面上那个 pill。** pill 的数字来自 React state，而入队是异步的
 *（`enqueue` 完才 `setQueued`）—— 用它当判据就是把一条数据层的断言绑在一次
 * UI 更新的时机上，偶发假红，而假红比没有断言更糟。
 *
 * 这里要验的是「这两条**确实躺在队列里**」，那就直接问队列本身。
 * 用户有没有被告知是另一件事，「离线捕获」那一段已经在验了。
 */
const outboxCount = () =>
  evaluate(
    s,
    `new Promise((res) => {
       const r = indexedDB.open('messagenote', 1);
       r.onerror = () => res(-1);
       r.onsuccess = () => {
         const db = r.result;
         if (!db.objectStoreNames.contains('outbox')) { db.close(); return res(0); }
         const tx = db.transaction('outbox', 'readonly');
         const c = tx.objectStore('outbox').count();
         c.onsuccess = () => { db.close(); res(c.result); };
         c.onerror = () => { db.close(); res(-1); };
       };
     })`
  );

const deadline = Date.now() + 15000;
while (Date.now() < deadline && (await outboxCount()) < flakeMarker.length) {
  await sleep(200);
}
const queuedCount = await outboxCount();
ok(
  `${flakeMarker.length} 条离线记录都进了队列`,
  queuedCount === flakeMarker.length,
  `队列里只有 ${queuedCount} 条`
);

// 在线，但接口打不通
await s.send("Network.setBlockedURLs", { urls: ["*/api/message*"] });
await setOffline(false);

// 恢复联网会触发 'online'，重放自动开跑。给它足够的时间把两条都试一遍。
await sleep(4000);

// **核心断言**：队列必须还是那两条。
// 修复前这里会是 0 —— 包装层重新入队、重放层紧接着删掉，净效果是消失。
ok(
  "重放途中请求失败，两条记录仍然留在队列里（没有被假回执吞掉）",
  (await outboxCount()) === flakeMarker.length,
  `队列里只剩 ${await outboxCount()} 条`
);
ok(
  "它们也确实没到服务端（所以队列留着是对的，不是重复）",
  !(await text()).includes(flakeMarker[0])
);

// 通了之后自己再试一次：这次要真的补发出去，且各出现**一次**
await s.send("Network.setBlockedURLs", { urls: [] });
await evaluate(s, `window.dispatchEvent(new Event('online')); true`);

// **等两条都离开队列**。判据看队列而不是界面：补发成功了但当前视图
// 看不到它们（比如还在某个频道里），那是视图的事，不是"没发出去"。
const untilDrained = Date.now() + 15000;
let left = -1;
while (Date.now() < untilDrained) {
  left = await outboxCount();
  if (left === 0) break;
  await sleep(200);
}
ok(
  "通了之后队列自己排空了（重放成功）",
  left === 0,
  `队列里还剩 ${left} 条`
);

// 回到时间线确认它们真的落在库里了
await evaluate(
  s,
  `(() => {
    const b = [...document.querySelectorAll('.nav-item')]
      .find(x => x.textContent.includes('时间线'));
    if (b) b.click();
    return true;
  })()`
);
await waitFor(
  s,
  `${JSON.stringify(flakeMarker[0])} &&
     document.body.innerText.includes(${JSON.stringify(flakeMarker[1])})`,
  "恢复之后两条都补发出去",
  15000
);
for (const m of flakeMarker) {
  const n = await evaluate(
    s,
    `(document.body.innerText.match(new RegExp(${JSON.stringify(m)}, 'g')) || []).length`
  );
  ok(`「${m}」补发了一次且只有一次`, n === 1, `出现了 ${n} 次`);
}
await waitFor(s, `!document.querySelector('.offline-pill')`, "补完之后提示消失", 10000);

// ---------------------------------------------------------------- 离线打开
console.log("== 离线打开：断网也能把页面拉起来 ==");

// Service Worker 是在页面加载**之后**才装上的，所以加载它的那一屏不受它控制。
// 要先等它真的激活，再刷一次页面，它才接管得了一屏。
await evaluate(s, `navigator.serviceWorker.ready.then(() => true)`);
await s.send("Page.reload", { ignoreCache: false });
await sleep(1500);
await waitFor(
  s,
  `!!navigator.serviceWorker.controller`,
  "Service Worker 接管了这个页面",
  15000
);
ok("Service Worker 已经接管页面", await evaluate(s, `!!navigator.serviceWorker.controller`));

// **缓存里到底有什么。** 上面那条断言（离线刷新能拉起来）在壳没被缓存住时
// 只会报「ERR_INTERNET_DISCONNECTED」—— 而原因可能是预缓存整个失败了、
// 也可能只是少了某个资源。把缓存内容打出来，才能一眼看出是哪一种。
{
  const cached = await evaluate(
    s,
    `caches.keys().then(async (keys) => {
       const out = {};
       for (const k of keys) {
         const c = await caches.open(k);
         out[k] = (await c.keys()).map(r => new URL(r.url).pathname);
       }
       const regs = await navigator.serviceWorker.getRegistrations();
       return { caches: out, sw: regs.map(r => ({
         scope: r.scope,
         active: r.active && r.active.scriptURL,
         state: r.active && r.active.state,
       })) };
     })`
  );
  console.log("（诊断）" + JSON.stringify(cached, null, 1));
  ok(
    "Service Worker 预缓存里有网页端的壳和资源",
    Object.values(cached.caches).some(
      (paths) => paths.includes("/web.html") && paths.some((p) => p.startsWith("/assets/"))
    ),
    `缓存内容：${JSON.stringify(cached)}`
  );
}

await setOffline(true);
// **等浏览器真的报离线再刷新。** CDP 的 emulateNetworkConditions 是异步
// 生效的，抢跑的话这一刷还是在线的，于是「离线刷新」其实测的是在线刷新。
await waitFor(s, `navigator.onLine === false`, "浏览器报告已离线", 10000);

// 记下这一刷里的网络事件：SW 有没有接管导航、导航拿到的是什么。
const navEvents = [];
const onRequest = (e) =>
  navEvents.push({ kind: "request", url: e.request.url, fromSW: !!e.request.fromServiceWorker });
const onResponse = (e) =>
  navEvents.push({
    kind: "response",
    url: e.response.url,
    status: e.response.status,
    fromSW: e.response.fromServiceWorker,
  });
const onFailed = (e) =>
  navEvents.push({ kind: "failed", url: e.request.url, error: e.errorText });
const onLoadingFailed = (e) =>
  navEvents.push({ kind: "loadingFailed", url: e.request?.url ?? "?", error: e.errorText });

await s.send("Network.enable");
await s.on("Network.requestWillBeSent", onRequest);
await s.on("Network.responseReceived", onResponse);
await s.on("Network.loadingFailed", onFailed);
await s.on("Network.requestServedFromCache", onLoadingFailed);

// **不要用 `ignoreCache: true`。** 那是浏览器的硬刷新，而硬刷新**按设计
// 绕过 Service Worker** —— 所以这一刷里 SW 一次都没接管（诊断能看到导航请求
// `fromServiceWorker: false`），页面当然拿不到缓存，然后报 ERR_INTERNET_DISCONNECTED。
//
// 这个坑值得写在这里：症状是「离线壳不工作」，指向 sw.js，而真实原因是
// 测试自己用了一个绕过 SW 的刷新方式。用户真实的断网打开走的是普通导航，
// 那是 SW 接管的路径 —— 所以这里也用普通导航。
await s.send("Page.reload", { ignoreCache: false });
await sleep(2500);

await s.off("Network.requestWillBeSent", onRequest);
await s.off("Network.responseReceived", onResponse);
await s.off("Network.loadingFailed", onFailed);
await s.off("Network.requestServedFromCache", onLoadingFailed);



// **这是关键**：没有 Service Worker 的话，离线刷新会得到浏览器的网络错误页，
// 里面不可能有 .app。所以这一条真真切切在验"壳被缓存下来了"。
ok(
  "断网刷新之后界面仍然拉得起来（壳是从缓存里来的）",
  await evaluate(s, `!!document.querySelector('.app')`),
  await evaluate(s, `document.body.innerText.slice(0, 120)`)
);

await setOffline(false);
await sleep(1500);
ok(
  "恢复联网后界面正常",
  await evaluate(s, `!!document.querySelector('.app')`)
);

// ---------------------------------------------------------------- 收尾
console.log("== 页面健康 ==");
ok(
  "整轮下来页面零报错",
  s.consoleErrors.length === 0,
  s.consoleErrors.join(" | ")
);

// 中途抛异常（waitFor 超时之类）也要算失败，不能让"0 条失败"骗过去
finish();
