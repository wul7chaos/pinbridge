# 总体架构

## 一句话

把"分析逻辑"从**固定接口的排列组合**变成**注入目标进程的脚本**，
用插装框架（而不是调试器框架）做基座，再用显式权限与可复现轨迹把
"AI 自主操作"这件事纳入可控范围。

## 分层

```text
┌──────────────────────────── 主体层（人机平级）────────────────────────────┐
│  AI 客户端（Claude Code / Cursor / 自研 agent）   人类（工作台 / CLI）      │
│            │ MCP stdio                                     │ Tauri invoke  │
├────────────┴──────────────────────────────────────────────────────────────┤
│  接入层     pinbridge-mcp（薄工具层）        apps/desktop（观察 + 接管）     │
│             pinbridge-cli（tools/replay/demo/mcp）                          │
├───────────────────────────────────────────────────────────────────────────┤
│  控制面     pinbridge-hub                                                   │
│             会话状态机 · 控制权租约(epoch) · 能力集 · 脚本生命周期           │
│             事件窗口 · 轨迹录制 · 操作溯源                                   │
├───────────────────────────────────────────────────────────────────────────┤
│  链路层     pinbridge-client（传输抽象 / 本机回环 TCP）                      │
│             pinbridge-proto（线路协议 · MCP 工具描述）                       │
├───────────────────────────────────────────────────────────────────────────┤
│  目标层     pinbridge-agent（注入目标进程的 cdylib）                         │
│             事件队列 · 序号分配 · 录制器 · CPython 脚本宿主 · 能力双校验      │
│                         │ pinbridge-sys（稳定 C ABI）                        │
│             pinbridge.dll（C++ / Intel Pin：探针 · JIT 上下文 · TF 虚拟化）   │
├───────────────────────────────────────────────────────────────────────────┤
│  离线层     python/（.pbt 解码 · 重放 · 结构化污点 · OEP 候选 · 反向切片）    │
└───────────────────────────────────────────────────────────────────────────┘
```

## 数据流（一次典型的 AI 分析回合）

```text
1. AI 调用 pb_session_list / pb_session_attach
        └─▶ hub：能力校验 → 控制权校验 → 会话状态迁移
2. AI 调用 pb_trace_record(start)           ← 先开录，再动手
        └─▶ hub：分配 .pbt 路径 → agent 打开录制器
3. AI 调用 pb_script_load(源码)
        └─▶ hub：能力(script_load) → epoch → 脚本 sha256 入溯源 → 下发 agent
        └─▶ agent：内嵌 CPython 加载，回调注册到订阅的事件类型
4. 目标运行 → 探针命中 → agent 采集事件
        └─▶ 有界通道（满了丢弃并计数）→ 折叠器 → .pbt（流式 chunk）
        └─▶ 同时留在 hub 的记录窗口里供回看
5. AI 调用 pb_events_query(from, limit)
        └─▶ hub：从窗口取 → 折叠成"代表 + REPEAT 标记" → 返回 views + dropped
6. AI 根据事件写出/替换脚本，回到 3
7. AI 调用 pb_trace_record(stop) → 得到 summary
        └─▶ 交给离线层：污点传播 / 反向切片 / OEP 候选
8. 人类随时可以在工作台看到全过程的审计链，并 takeover 拿回控制权
```

## 关键设计取舍

### 为什么基座是插装框架而不是调试器框架

调试器（Windows 调试框架）依赖异常机制，目标是**可以主动检测并干扰**的；
而且下断点/单步本身就会改变时序，容易被针对。插装框架把目标代码加载进自己的
代码缓存运行，能做到**指令级**控制而不依赖被分析进程的异常路径。
详见 [ADR-0001](../adr/0001-instrumentation-over-debug-api.md)。

### 为什么"AI 主导"而不是"AI 辅助"

插件式方案里 AI 只能调用预先包装好的固定接口，遇到了解不到的中间状态就断了。
PinBridge 让 AI **写代码**：判断、循环、试错、回退都在脚本里完成，
MCP 工具只保留"开门/观察/收尾"三类薄动作。
详见 [ADR-0002](../adr/0002-script-first-over-tool-menu.md)。

### 为什么一定要落轨迹

Hub 的事件窗口会滚、通道会丢、目标可能崩。**只有 `.pbt` 是完整的**。
所以框架里有一条硬规矩：任何"结论"都必须能指回一份轨迹。
详见 [../design/trace-compression.md](../design/trace-compression.md) 与
[../specs/pbt-format.md](../specs/pbt-format.md)。

### 为什么权限要显式到这种程度

因为执行者是 AI。人类不会"不小心"在 60 秒里尝试 200 次越权操作，
AI 会。显式能力集 + epoch + 双端校验 + 溯源，都是为了把"AI 的高频试错"
控制在不会伤害目标的范围内。
详见 [control-ownership.md](control-ownership.md)。

## 与原文架构的对应关系

原文（看雪 thread-293115）给出的三层与组件，在本仓库的落点：

| 原文概念 | 本仓库实现 |
|----------|-----------|
| AI 分析器 / 人类分析员 | `Principal`（`ai:` / `human:`，**平级**） |
| MCP 能力接口 | `crates/pinbridge-proto/src/mcp.rs` + `pinbridge-mcp` |
| 人工工作台 | `apps/desktop`（Tauri + React） |
| 会话与控制层 | `pinbridge-hub`（`HubService`） |
| 动态脚本环境 | `pinbridge-agent` 内嵌 CPython（M1） |
| 声明规则与处理决策 | 脚本的订阅集 + 断点/Hook 声明 |
| 原生分析内核（插桩/断点/上下文） | `pinbridge.dll`（C++）+ `pinbridge-agent` |
| 有界事件通道 | `pinbridge_core::channel::bounded`（丢弃计数必上报） |
| 执行轨迹录制 | `pinbridge-trace::pbt` + `pinbridge-hub::TraceRecorder` |
| 离线重放与深度分析 | `python/src/pb/trace/{pbt,replay}.py` |
| `.pbt` 轨迹文件 | [../specs/pbt-format.md](../specs/pbt-format.md) |
| TF 虚拟化 | [flags-virtualization.md](../design/flags-virtualization.md)（M1 落地） |
| trace 压缩（重复折叠） | `pinbridge-trace::fold`（**已实现且可逆**） |

逐条差异（包括我们主动改掉的地方）见
[../research/inspiration-and-differences.md](../research/inspiration-and-differences.md)。
