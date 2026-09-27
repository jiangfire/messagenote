/**
 * 同源验证服务器：托管 dist/，并把 /api/* 反代到 Rust 服务端。
 *
 * 这是为了**验证**同源部署真的可行 —— 生产用的是 `deploy/Caddyfile` 里的
 * Caddy 配置（本机没装 Caddy，也不想为了测试去下一个）。
 * 行为要点和那份配置一致：`/api/*` 走服务端，`/assets/*` 走静态文件，
 * 其余一律回 `web.html`（单页应用）。
 *
 * 用法：node serve.mjs <dist 目录> <API 地址> <端口>
 */
import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize, resolve } from "node:path";

const DIST = resolve(process.argv[2]);
const API = process.argv[3];
const PORT = Number(process.argv[4] ?? 8804);

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
};

async function serveFile(res, file) {
  try {
    const s = await stat(file);
    if (!s.isFile()) throw new Error("not a file");
    res.writeHead(200, {
      "Content-Type": TYPES[extname(file)] ?? "application/octet-stream",
      "Content-Length": s.size,
    });
    res.end(await readFile(file));
  } catch {
    res.writeHead(404, { "Content-Type": "text/plain; charset=utf-8" });
    res.end("not found");
  }
}

createServer(async (req, res) => {
  const url = new URL(req.url, "http://localhost");

  if (url.pathname.startsWith("/api/")) {
    const upstream = await fetch(API + req.url, {
      method: req.method,
      headers: req.headers,
      // 流式转发请求体；Node 的 fetch 要求显式声明 half duplex
      body: req.method === "GET" || req.method === "HEAD" ? undefined : req,
      duplex: "half",
    });
    const buf = Buffer.from(await upstream.arrayBuffer());
    const headers = {};
    for (const [k, v] of upstream.headers) {
      // 这几个由我们自己重算，转发过去反而会不一致
      if (["content-encoding", "content-length", "transfer-encoding"].includes(k)) continue;
      headers[k] = v;
    }
    headers["Content-Length"] = buf.length;
    res.writeHead(upstream.status, headers);
    res.end(buf);
    return;
  }

  // 静态文件。路径必须留在 DIST 里面 —— 少一个 ../ 就是目录穿越。
  const rel = normalize(url.pathname).replace(/^([/\\]|\.\.[/\\])+/, "");
  const file = join(DIST, rel);
  if (!file.startsWith(DIST)) {
    res.writeHead(403).end("forbidden");
    return;
  }

  if (url.pathname.startsWith("/assets/")) {
    await serveFile(res, file);
    return;
  }

  // 其余一律回网页端入口。
  // **注意是 web.html 不是 index.html** —— 后者是 Tauri 主窗口的壳，
  // 依赖 window.__TAURI__，在浏览器里打开只会白屏。
  await serveFile(res, join(DIST, "web.html"));
}).listen(PORT, "127.0.0.1", () => {
  console.log(`同源服务器已启动：http://127.0.0.1:${PORT}  (dist=${DIST}, api=${API})`);
});
