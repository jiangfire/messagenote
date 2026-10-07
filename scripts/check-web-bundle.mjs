/**
 * 检查网页端的 bundle 里没有混进 Tauri。
 *
 * ## 为什么需要它
 *
 * 网页端和桌面端跑的是同一套界面，靠 `useApi()` 注入不同的数据层。只要有人在
 * 共享的组件树里直接 `import` 了 `lib/api`（那里面有 `@tauri-apps/api`），
 * Tauri 的 IPC 代码就会进网页端的 bundle。
 *
 * 这个错误最阴的地方在于：**构建成功、类型检查通过、桌面端照跑**，
 * 只有去看产物才发现 —— 网页端白白多下载一份用不上的代码，
 * 而"网页端不依赖 Tauri"这条边界已经被悄悄破掉了。
 *
 * 所以它值得一个能重复跑的检查，而不是靠某次手工 grep。
 *
 * 用法：node scripts/check-web-bundle.mjs [dist 目录]
 */
import { readFile, readdir } from "node:fs/promises";
import { existsSync } from "node:fs";
import { join, basename } from "node:path";

const DIST = process.argv[2] ?? "dist";

/** 在产物里找这些字样就算命中。`__TAURI` 是 Tauri 注入的全局前缀。 */
const NEEDLES = ["__TAURI", "@tauri-apps"];

/** 从一份 JS 里抽出它 import 的其他 chunk。 */
function importsOf(source) {
  const out = new Set();
  for (const m of source.matchAll(/from\s*["']\.\/([^"']+\.js)["']/g)) {
    out.add(m[1]);
  }
  return [...out];
}

async function readChunk(name) {
  const path = join(DIST, "assets", name);
  if (!existsSync(path)) return null;
  return readFile(path, "utf8");
}

async function main() {
  const htmlPath = join(DIST, "web.html");
  if (!existsSync(htmlPath)) {
    console.error(`找不到 ${htmlPath} —— 先跑 pnpm build`);
    process.exit(1);
  }

  const html = await readFile(htmlPath, "utf8");
  // Vite 会把入口脚本和它的静态依赖分别写成 <script src> 和 <link modulepreload>
  const entries = [...html.matchAll(/(?:src|href)="\/assets\/([^"]+\.js)"/g)].map(
    (m) => m[1]
  );
  if (entries.length === 0) {
    console.error("web.html 里没有找到任何 JS 入口 —— 产物结构变了？");
    process.exit(1);
  }

  // 从入口出发把整张图走一遍。modulepreload 通常已经列全了，
  // 但跟着 import 走一遍才不依赖 Vite 的实现细节。
  const seen = new Set();
  const queue = [...entries];
  const hits = [];

  while (queue.length > 0) {
    const name = queue.shift();
    if (seen.has(name)) continue;
    seen.add(name);

    const source = await readChunk(name);
    if (source === null) continue;

    for (const needle of NEEDLES) {
      if (source.includes(needle)) {
        hits.push(`${name} 含有 ${needle}`);
      }
    }
    queue.push(...importsOf(source));
  }

  console.log(`检查了 ${seen.size} 个 chunk：${[...seen].sort().join(", ")}`);

  if (hits.length > 0) {
    console.error("\n网页端可达的 chunk 里混进了 Tauri：");
    for (const h of hits) console.error(`  - ${h}`);
    console.error(
      "\n多半是共享组件树里有人直接 import 了 lib/api。" +
        "改用 useApi() 注入的 api / desktop，见 src/lib/apiContext.tsx。"
    );
    process.exit(1);
  }

  // 顺带确认桌面端入口**仍然**带着 Tauri —— 上面那个检查要是因为
  // 产物结构变了而什么都没查到，这条会立刻暴露出来。
  const all = await readdir(join(DIST, "assets"));
  const tauriChunk = [];
  for (const f of all.filter((f) => f.endsWith(".js"))) {
    const s = await readChunk(f);
    if (s && NEEDLES.some((n) => s.includes(n))) tauriChunk.push(f);
  }
  if (tauriChunk.length === 0) {
    console.error(
      "\n桌面端那边也找不到 Tauri 的痕迹 —— 说明这个检查本身失效了，不是好事。"
    );
    process.exit(1);
  }
  console.log(`桌面端仍然带着 Tauri（${tauriChunk.join(", ")}），检查有效。`);

  await checkServiceWorker();
  console.log("网页端 bundle 干净。");
}

/**
 * Service Worker 必须真的被打进产物、并且带着**这次构建**的版本号。
 *
 * 两个坑都在这里出现过：
 *
 * - **没被打进去。** `public/` 是原样拷的，但两条文档化的部署路径
 *   （deploy/Caddyfile 的安装指引、deploy/web.Dockerfile 的 COPY）曾经
 *   只拷 `assets` 和 `web.html`。于是生产 `GET /sw.js` 掉进 SPA 兜底，
 *   返回 200 + text/html，浏览器以 MIME 不符拒绝注册 —— **离线壳在所有
 *   按文档部署的环境里静默失效**，报的还是 "unsupported MIME type"。
 *   scripts/web-e2e/serve.mjs 早就修过一模一样的坑（那边有注释），生产配置漏了。
 *
 * - **版本号是死的。** `VERSION` 曾经硬编码，于是 install 永不重跑、
 *   activate 的旧缓存清理永不生效 —— 「版本更新后清缓存」是个不存在的功能。
 *
 * 所以这里检查产物里的 sw.js：**存在**、**真的换了版本**、且这个版本
 * 和 package.json 对得上。
 */
async function checkServiceWorker() {
  const swPath = join(DIST, "sw.js");
  if (!existsSync(swPath)) {
    console.error(
      "\n产物里没有 sw.js —— Service Worker 只从根路径 /sw.js 加载，" +
        "少了它离线打开就是浏览器的错误页。"
    );
    process.exit(1);
  }

  const { version } = JSON.parse(await readFile("package.json", "utf8"));
  const sw = await readFile(swPath, "utf8");

  if (sw.includes("__MESSAGENOTE_BUILD__")) {
    console.error(
      "\nsw.js 里的版本占位符没被替换 —— pnpm build 是不是漏了 stamp-sw-version.mjs？"
    );
    process.exit(1);
  }

  const m = sw.match(/const VERSION\s*=\s*(".*?"|null)/);
  if (!m || !m[1].includes(version)) {
    console.error(
      `\nsw.js 的版本串（${m ? m[1] : "没找到"}）里没有当前的 ${version} —— ` +
        "版本不变的话 install 不会重跑、activate 也不会清旧缓存。"
    );
    process.exit(1);
  }

  // 顺带确认源码里是占位符：否则有人把版本又写死了，构建照样"通过"，
  // 而 VERSION 从此不再随发版变化。
  const source = await readFile("public/sw.js", "utf8");
  if (!source.includes("__MESSAGENOTE_BUILD__")) {
    console.error(
      "\npublic/sw.js 里没有版本占位符 —— 版本号又被写死了，" +
        "那样每次发版的缓存清理都不会发生。"
    );
    process.exit(1);
  }

  console.log(`sw.js 存在，版本已随构建更新（v${version}）。`);
}

await main();
