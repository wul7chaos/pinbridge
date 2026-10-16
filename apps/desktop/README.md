# apps/desktop —— 人类的位置

> **状态：数据层已接真实产物，界面接线待做。**
>
> * **真实数据层**：`pinbridge-workbench`（Rust crate，带用例）读真实 `.pbt`
>   产出首屏载荷；CLI 入口 `pinbridge view <轨迹> [--json]` 与
>   `pinbridge capabilities [--json]`。**告警永远排在数据之前** ——
>   残缺的结论与完整的结论长得一样。
> * **真实客户端**：`src/api/ndjson.mjs`（Hub 的 NDJSON 实现，附 `.d.ts`），
>   7 条 `node --test` 用例覆盖分帧、请求关联、错误传播、`dropped` 强制、
>   回环约束。**门禁里会跑**（第 11 步），CI 有独立 job。
> * **视图模型**：`src/state/view-model.mjs`（零依赖，7 条 `node --test` 用例）
>   把 README 里下面那三条硬要求**写成了代码**：告警永远第一个区块、`dropped`
>   常驻（含原始计数）、租约与 epoch 常驻、接管按钮永远在。组件只负责渲染它。
> * **演示数据必须自报家门**：`useViewPayload` 优先走 Tauri 命令；拿不到就退到演示
>   数据，但会**在告警里写明"当前是演示数据"** —— 演示数据与真实数据长得一样，
>   这是这个界面最容易骗到人的地方。
> * **仍未做**：把 Tauri 壳装起来（`src-tauri/` 的命令薄包装）。
>   Tauri/React 的**构建**刻意不进 CI（需要 Node 依赖 + 系统 WebView，
>   会让门禁从几十秒变成十分钟）；只验证不依赖 registry 的那部分逻辑。

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
