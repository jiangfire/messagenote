import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { ApiProvider } from "./lib/apiContext";
import { tauriApi, tauriDesktop, tauriSubscribeChanges } from "./lib/tauriApi";
import "./styles.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("找不到 #root 挂载点");
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    {/* `subscribeChanges` **不能漏**：同步把远端变更落库之后，界面靠它重取一次。
        漏了的话库是对的、同步也是快的，而时间线上永远看不见 —— 见
        `tauriSubscribeChanges` 的说明。 */}
    <ApiProvider
      api={tauriApi}
      desktop={tauriDesktop}
      subscribeChanges={tauriSubscribeChanges}
    >
      <App />
    </ApiProvider>
  </React.StrictMode>
);
