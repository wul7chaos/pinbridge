import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Vite 配置有意保持最小。
//
// 注意：**不是所有嵌入方式都等于"生产构建"**。Tauri 会把 dist/ 打包进壳里，
// 所以在接真实数据之前，`npm run dev` 只服务 mock 数据。
export default defineConfig({
  plugins: [react()],
  // Tauri 的约定：不要在构建产物里暴露 dev server 的地址
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    // Tauri 使用的 WebView 版本较新，可以直接用现代语法
    target: "esnext",
    sourcemap: true,
  },
});
