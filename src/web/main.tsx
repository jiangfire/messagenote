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
