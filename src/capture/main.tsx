import React from "react";
import ReactDOM from "react-dom/client";
import CaptureApp from "./CaptureApp";
import "./capture.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("找不到 #root 挂载点");
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <CaptureApp />
  </React.StrictMode>
);
