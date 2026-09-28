/*
 * Service Worker：让**没网的时候也能打开**这个页面。
 *
 * 没有它的话，离线捕获只在"页面已经开着"时成立 —— 手机在地铁里锁屏之后再
 * 点开，浏览器连 HTML 都拿不到，直接是浏览器的错误页。那样"零摩擦捕获"
 * 在最需要它的场景里恰好不工作。
 *
 * ## 缓存策略为什么是这两种
 *
 * - **页面（导航）：网络优先，失败回缓存。** 不能用缓存优先 ——
 *   `web.html` 的路径是固定的，而它引用的资源名是带哈希的。
 *   缓存优先会让用户永远停在一个引用着已被删掉的 JS 的旧页面上，
 *   表现是白屏，而且刷新也没用（刷新还是走缓存）。
 * - **`/assets/*`：缓存优先。** 这些文件名带内容哈希，内容永远不会变
 *   （改了名字就变了），所以缓存优先是安全的，也是最快的。
 * - **`/api/*`：一律不碰。** 缓存同步数据会让客户端读到过期状态，
 *   而"同步悄悄不对"是最难查的一类 bug。
 *
 * 这个文件刻意是**不经过打包的普通 JS**，放在 `public/` 里原样拷进 `dist/`。
 * Service Worker 必须从一个**稳定的 URL** 加载（`/sw.js`），
 * 而 Vite 给入口文件的名字是带哈希的。它也不需要任何依赖。
 */

const VERSION = "messagenote-shell-1";
const SHELL = "/web.html";

self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(VERSION);
      await cache.add(SHELL);

      // 顺手把页面引用的资源也抓下来。
      //
      // 只缓存 HTML 是不够的：SW 是在页面加载**之后**才装上的，那时
      // CSS/JS 早就请求完了，运行时的缓存策略永远轮不到它们。
      // 不预缓存的话，装好之后立刻断网刷新就是一个没有样式的空壳。
      const res = await cache.match(SHELL);
      if (res) {
        const html = await res.text();
        const assets = [...html.matchAll(/["'](\/assets\/[^"']+)["']/g)].map(
          (m) => m[1]
        );
        await cache.addAll(assets);
      }

      await self.skipWaiting();
    })()
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      // 清掉旧版本的缓存。不清的话它们永远不会被回收。
      const keys = await caches.keys();
      await Promise.all(keys.filter((k) => k !== VERSION).map((k) => caches.delete(k)));
      await self.clients.claim();
    })()
  );
});

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  // 跨域的（比如笔记正文里的外链图片）一概不管
  if (url.origin !== self.location.origin) return;
  // API 绝不缓存
  if (url.pathname.startsWith("/api/")) return;

  if (request.mode === "navigate") {
    event.respondWith(
      (async () => {
        try {
          return await fetch(request);
        } catch {
          // 断网了。回缓存的壳 —— 单页应用的路由在客户端，
          // 所以任何路径都回同一个壳。
          const cached = await caches.match(SHELL);
          if (cached) return cached;
          throw new Error("离线，而且壳也没缓存下来");
        }
      })()
    );
    return;
  }

  if (url.pathname.startsWith("/assets/")) {
    event.respondWith(
      (async () => {
        const cached = await caches.match(request);
        if (cached) return cached;
        const res = await fetch(request);
        // 只缓存成功的。把 404 也存下来的话，那个资源就**永远**是 404 了。
        if (res.ok) {
          const cache = await caches.open(VERSION);
          cache.put(request, res.clone());
        }
        return res;
      })()
    );
  }
});
