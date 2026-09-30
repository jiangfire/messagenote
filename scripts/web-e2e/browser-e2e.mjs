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

await setOffline(true);
await s.send("Page.reload", { ignoreCache: true });
await sleep(2500);

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

console.log(failed ? `\n${failed} 条失败` : "\n全部通过");
process.exit(failed ? 1 : 0);
