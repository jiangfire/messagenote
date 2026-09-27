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
  console.log("网页端 bundle 干净。");
}

await main();
