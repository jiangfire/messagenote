/**
 * 把 Service Worker 里的版本占位符换成真实的发版号。
 *
 * ## 为什么必须有这一步
 *
 * `public/sw.js` 是**原样拷进 `dist/`** 的，不经过 Vite 的 define/替换 ——
 * 这是必须的，Service Worker 要从稳定的 `/sw.js` 加载。
 * 代价就是里面的常量在源码里写死。
 *
 * 而那个常量是**缓存的名字**，也是「这个缓存是不是当前的」判据：
 * - 写死 → install 永远不重跑（浏览器只看文件有没有变），
 *   activate 里的旧缓存清理也永远不生效。
 *   于是「版本更新后清掉旧缓存」是一个**看起来存在、实际从不发生**的功能，
 *   而且没有任何报错。
 * - 每次发版变一次 → 新 SW 被安装、activate 清掉旧缓存、壳更新。
 *
 * 放在 `pnpm build` 之后跑：那时 `dist/sw.js` 已经存在。
 *
 * 用法：node scripts/stamp-sw-version.mjs [dist 目录]
 */
import { readFile, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DIST = resolve(process.argv[2] ?? join(ROOT, "dist"));
const SW = join(DIST, "sw.js");

const PLACEHOLDER = "__MESSAGENOTE_BUILD__";

if (!existsSync(SW)) {
  console.error(`找不到 ${SW} —— 先跑 vite build`);
  process.exit(1);
}

const { version } = JSON.parse(
  await readFile(join(ROOT, "package.json"), "utf8")
);

const source = await readFile(SW, "utf8");

if (!source.includes(PLACEHOLDER)) {
  // 不当成错误：改了占位符名字的话这里会一直报错，
  // 而"没找到要替换的东西"对构建来说不是失败。
  console.log("sw.js 里没有版本占位符，跳过（可能已经被替换过了）。");
  process.exit(0);
}

await writeFile(SW, source.replaceAll(PLACEHOLDER, `"v${version}"`));

const stamped = await readFile(SW, "utf8");
if (!stamped.includes(`v${version}`)) {
  console.error(`替换没有生效：sw.js 里找不到 v${version}`);
  process.exit(1);
}
console.log(`sw.js 版本已标为 v${version}（缓存名会随之变化，旧缓存在 activate 时被清掉）。`);