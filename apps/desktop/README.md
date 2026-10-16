# apps/desktop —— 人类的位置

> **状态：M0 骨架。** 界面组件与结构已经定义，但**没有接线到真实数据**，
> 也没有在本仓库的 CI 中构建（Tauri 构建需要 Rust + Node + 系统 WebView，
> 把它塞进每次 CI 会让门禁从 40 秒变成 10 分钟）。
> 接线计划见 [../../docs/roadmap.md](../../docs/roadmap.md) 的 M3。

## 这里是谁用的

人类的角色只有两个：**观察**与**接管**。

| 人类不做 | 人类做 |
|----------|--------|
| 一条条下命令去驱动分析 | 设定目标与边界（授权、ceiling） |
| 盯着事件流做推理 | 看 AI 在做什么、跑到哪一步 |
| 手工下断点 | 必要时 `takeover` 拿回控制权 |

这不是"人退居二线"的修辞，而是架构上的分工：
AI 是执行主体（能写脚本、能试错），人类是**阀门**与**审计者**。

## 界面结构（计划）

```text
┌─────────────────────────────────────────────────────────────────┐
│ 会话栏：状态 · 目标 · PID · 控制权持有者(与 epoch) · 录制状态      │
├──────────────┬──────────────────────────────────────────────────┤
│ 左：脚本与溯源 │ 中：事件流（折叠后的记录，可展开重复段）           │
│  · 当前脚本    │      · 类型过滤 · 地址范围 · 跟随最新              │
│  · 版本历史    │                                                  │
│  · 审计链      │                                                  │
├──────────────┴──────────────────────────────────────────────────┤
│ 底：能力矩阵 + 接管按钮 + 轨迹摘要（records/events/folded/dropped）│
└─────────────────────────────────────────────────────────────────┘
```

设计与实现要点（都写在组件注释里）：

* **`dropped` 必须常驻可见**：底部状态条上永远显示它，
  非零时变黄。人不能"不知道这份结论建立在丢过数据的基础上"；
* **控制权持有者与 epoch 常驻可见**：一眼看出现在是 AI 还是人在驾驶；
* **接管按钮不做二次确认**：接管本身是可恢复的（AI 重新 attach 即可），
  加确认弹窗只会让人在需要它的那一刻犹豫；
* **审计链可搜索**：按 actor（`ai:` / `human:`）与 action 过滤。

## 目录

```text
apps/desktop/
  package.json          Tauri + React + Vite（依赖面刻意很小）
  vite.config.ts
  tsconfig.json
  index.html
  src/
    main.tsx            入口
    App.tsx             三栏布局（骨架，使用本地 mock）
    styles.css          暗色配色：这个界面要在深夜看几小时
    api/hub.ts          Hub 调用的类型化包装（MCP/JSON IPC）
    components/
      SessionBar.tsx    会话与录制状态
      EventStream.tsx   事件流（折叠记录 + 展开）
      AuditChain.tsx    审计链
      CapabilityPanel.tsx 能力矩阵（可见 + 可授予）
```

## 与 Hub 的连接方式

两种，都能用：

| 方式 | 路径 | 适用 |
|------|------|------|
| Tauri 进程内调用 | `tauri::command` → `HubService` | 桌面端自带的 Hub（单机分析） |
| 本机 JSON IPC | 回环 TCP + `ipc-protocol.md` 的 NDJSON | 桌面端连一个已在跑的 Hub |

**只允许回环**（同 `pinbridge-client` 的约束）：把调试能力暴露到网络上
不是可配置项，见 [../../docs/architecture/security-model.md](../../docs/architecture/security-model.md)。

## 本地开发

```bash
cd apps/desktop
npm install
npm run dev          # 只跑前端（mock 数据）
npm run tauri dev    # 带上 Tauri 壳（需要 Rust 工具链）
```

M0 阶段 `npm run dev` 会显示 mock 数据；真实接线在 M3。
