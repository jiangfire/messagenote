import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 在开发期会注入 TAURI_DEV_HOST（仅移动端调试时非空）
const host = process.env.TAURI_DEV_HOST;

// 多入口：主窗口和捕获浮层是两个独立的 HTML。
// 浮层单独打包是有意的 —— 它必须在主界面还没渲染出来之前就能用。
const root = fileURLToPath(new URL(".", import.meta.url));

export default defineConfig({
  plugins: [react()],

  // Tauri 自己会打印构建信息，别让 vite 清屏把 Rust 的报错冲掉
  clearScreen: false,

  server: {
    port: 1420,
    // 端口被占用时直接失败，而不是悄悄换端口 —— 否则 Tauri 会连到一个空窗口
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: {
      // 这些目录都由别的工具负责，vite 不该插手：
      // - src-tauri / crates 由 cargo 自己 watch，vite 再来一遍会导致重复重启
      // - target 是 workspace 的构建产物目录，里面全是被 cargo 锁住的
      //   .dll/.exe；watch 它会直接以 EBUSY 崩掉整个 dev server
      // - .cargo / .pnpm-store 是依赖缓存，动辄几万个文件
      ignored: [
        "**/src-tauri/**",
        "**/crates/**",
        "**/target/**",
        "**/.cargo/**",
        "**/.pnpm-store/**",
      ],
    },
  },

  envPrefix: ["VITE_", "TAURI_ENV_"],

  build: {
    // Windows 上 WebView2 是 Chromium，可以放心用较新的语法
    target: "es2021",
    sourcemap: false,
    // 注意：不要写 minify: "esbuild"。
    // Vite 8 的构建链已经换成 rolldown + oxc，esbuild 被移出内置依赖，
    // 显式指定它会因找不到 esbuild 包而直接构建失败。留空走默认的 oxc 即可。
    rollupOptions: {
      input: {
        main: `${root}index.html`,
        capture: `${root}capture.html`,
      },
    },
  },
});
