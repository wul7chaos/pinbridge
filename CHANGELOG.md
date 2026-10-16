# 变更日志

本文件记录**对外可见**的变更（对使用者的行为有影响的一切）。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。
`.pbt` 格式版本（`FORMAT_VERSION`）与 C ABI 版本（`PB_ABI_VERSION`）
与包版本**独立演进**，任何一处变动都会单独标注。

## [Unreleased]

### 计划中（M1）
- `pinbridge.dll` 接通 Intel Pin：JIT 探针、上下文控制、TF 虚拟化。
- agent 内嵌 CPython 3.x：脚本加载/替换/卸载与执行时间预算。
- 真实内存读写、断点、单步、系统调用 Hook。

## [0.1.0] - 2026-10-16

M0：**工程骨架**。目标不是功能完备，而是"把规范、边界与验证方式先钉死"。

### 新增
- Rust 工作区（9 个 crate）：`core` / `proto` / `trace` / `hub` /
  `client` / `sys` / `agent` / `mcp` / `cli`，依赖方向单向。
- `.pbt` 轨迹格式 **v1**：chunk 容器 + delta/varint/zigzag 记录编码 +
  重复折叠标记；Rust 与 Python **双实现**共享同一组黄金字节用例。
- 重复折叠压缩：序号连续 + 载荷相同才折叠；压缩**可逆**（有测试）。
- 控制权模型：`Principal`（人与 AI 平级）、`CapabilitySet`（只减不增）、
  `ControlLease`（epoch 失效在途命令）。
- 会话状态机、有界事件通道（丢弃计数必须上报）、脚本生命周期与 sha256 溯源。
- MCP 服务：`initialize` / `tools/list` / `tools/call`，工具表按当前能力过滤，
  错误里带 `hint` 告诉 AI 下一步怎么办。
- CLI：`tools` / `replay` / `demo` / `mcp` / `version`，全部**离线可跑**。
- Python 侧：`.pbt` 解码、重放、结构化污点传播、OEP 候选定位、`pb-replay` 命令。
- 文档：架构/设计/规范/开发流程/ADR，共 20+ 篇，含对原文架构的逐条对照。
- 工程化：`rust-toolchain.toml` 钉版本、clippy pedantic 门禁、`deny.toml`
  供应链策略、CI（格式/lint/测试/端到端/Python/审计）、Issue 与 PR 模板。
- 端到端自检 `pinbridge demo`：建会话 → 拒绝越权 → 授权 → 装脚本 → 录轨迹 →
  折叠查询 → 读回校验 → 打印溯源。

### 已知限制
- 所有需要 Pin 运行时支撑的动作是**桩实现**，显式返回 `not_supported`。
- agent 内嵌 CPython 尚未接通；`pb_load_script` 只做校验与报错。
- Hub 与 agent 之间的回环 TCP 链路已定义并实现传输抽象，真实握手见 M1。
- 桌面工作台（`apps/desktop`）只有骨架，未接真实数据。

### 不兼容变更
无（首个版本）。

<!--
提交 PR 时请顺手在 [Unreleased] 下加一条。CI 会检查含代码变更的 PR 是否
动过本文件（只动 docs/ 的 PR 可跳过）。
-->
