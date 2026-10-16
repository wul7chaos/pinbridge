import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

const container = document.getElementById("root");
if (!container) {
  // 挂载点缺失是打包配置问题，直接抛错比静默空白更容易定位
  throw new Error("找不到 #root 挂载点：检查 index.html 与 vite 配置");
}

ReactDOM.createRoot(container).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
