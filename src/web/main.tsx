import React from "react";
import ReactDOM from "react-dom/client";
import WebApp from "./WebApp";
import "../styles.css";
import "./web.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("找不到 #root 挂载点");
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <WebApp />
  </React.StrictMode>
);

// 注册 Service Worker：让**没网的时候也能打开**这个页面。
//
// 没有它，离线捕获只在"页面已经开着"时成立 —— 手机在地铁里锁屏之后再点开，
// 浏览器连 HTML 都拿不到。那样"零摩擦捕获"在最需要它的场景里恰好不工作。
//
// 只在 https 或 localhost 下注册。其它情况（明文 HTTP 的内网地址）浏览器
// 根本不提供 serviceWorker，`navigator.serviceWorker` 会是 undefined。
if ("serviceWorker" in navigator) {
  // 加载完再注册：它和首屏渲染抢资源没有意义，而首屏更重要。
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch(() => {
      // 注册失败不该影响任何功能 —— 页面本身照常工作，只是离线打不开。
      // 这里刻意不提示用户：他没有能做的事，而"离线用不了"这件事
      // 只有真的离线时才会被发现。
    });
  });
}
