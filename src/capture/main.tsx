import React from "react";
import ReactDOM from "react-dom/client";
import CaptureApp from "./CaptureApp";
import { ApiProvider } from "../lib/apiContext";
import { tauriApi, tauriDesktop } from "../lib/tauriApi";
import "./capture.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("找不到 #root 挂载点");
}

// 浮层同时需要「写一条」（走 NoteApi）和「收起窗口」（桌面端专有），
// 所以两个都传进去 —— 和主界面用的是同一套注入方式。
ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <ApiProvider api={tauriApi} desktop={tauriDesktop}>
      <CaptureApp />
    </ApiProvider>
  </React.StrictMode>
);
