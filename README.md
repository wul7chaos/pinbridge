# PinBridge

> **面向 AI 的插装式动态分析框架** —— AI 主导、人机平级、代码驱动。
>
> 灵感来源：[看雪论坛《AI 时代逆向分析：动态分析框架》](https://bbs.kanxue.com/thread-293115.htm)（作者 BitWarden）。
> 我们把文章里的架构意图落成可构建、可测试、可协作的工程结构，
> 并在 [docs/research/inspiration-and-differences.md](docs/research/inspiration-and-differences.md)
> 里逐条写明"哪些是原文的、哪些是我们自己定的"。

[![CI](https://github.com/wul7chaos/pinbridge/actions/workflows/ci.yml/badge.svg)](https://github.com/wul7chaos/pinbridge/actions/workflows/ci.yml)
![license](https://img.shields.io/badge/license-MIT-blue)
![rust](https://img.shields.io/badge/rust-1.98.1-orange)
![python](https://img.shields.io/badge/python-%3E%3D3.10-blue)

---

## 这解决什么问题

传统调试器（x64dbg 等） + MCP 插件的组合，AI 只是"外挂"：它只能调用被预先
包装好的固定接口，主导权仍在人手里，而且它**看不到自己调用的接口到底做了什么**。
遇到脱壳这类"先判断 OEP → 再决定 Dump 时机 → 再修内存属性"的组合拳，
就只能在 token 里硬拼，还容易返工。

PinBridge 的立场是**把主导权交给 AI，把最终阀门留给人**：

| 维度 | 传统方案 | PinBridge |
|------|----------|-----------|
| 基座 | Windows 调试框架（异常机制，易被针对） | 插装框架（Pin/DynamoRIO），指令级可控 |
| AI 的表达方式 | 调用固定函数接口 | **写脚本注入目标进程**，任意循环/分支/试错 |
| 主导权 | 人主导，AI 辅助 | AI 主导，人类是观察员 + 接管者 |
| 权限 | 隐式（谁能调谁就能做） | 显式能力集 + 控制权 epoch + 全程溯源 |
| 证据 | 内存里的现场 | 可复现的 `.pbt` 轨迹（唯一真相来源） |

## 架构一览

```text
        AI 客户端（Claude Code / Cursor / 自研 agent）
                    │  MCP stdio
                    ▼
   ┌──────────────────────────────────────────┐
   │  pinbridge-mcp      MCP 工具层（薄）      │
   │  pinbridge-cli      CLI / TUI            │
   │  apps/desktop       Tauri + React 工作台  │
   └───────────────────┬──────────────────────┘
                       ▼
   ┌──────────────────────────────────────────┐
   │  pinbridge-hub      会话 · 控制权 · 脚本   │
   │                     · 事件窗口 · 溯源      │
   └───────────────────┬──────────────────────┘
                       │  pinbridge-client（本机回环 TCP）
                       ▼
   ┌──────────────────────────────────────────┐
   │  pinbridge-agent    注入目标进程（cdylib） │
   │  事件队列 · 录制器 · CPython 脚本宿主      │
   └───────────────────┬──────────────────────┘
                       │  pinbridge-sys（稳定 C ABI）
                       ▼
        pinbridge.dll（C++ / Intel Pin tool）→ 目标进程 x64 / x86
```

细节见 [docs/architecture/overview.md](docs/architecture/overview.md)。

## 快速开始（不需要 Pin，也不需要目标程序）

本仓库的 M0 骨架**完全离线可跑**：没有 Pin 时一切需要插装的动作都会显式返回
`not_supported`，而不是静默成功。

```bash
git clone https://github.com/wul7chaos/pinbridge
cd pinbridge

# Rust 侧：构建 + 测试（139 个单测）
cargo test --workspace

# 质量门禁（与 CI 完全一致）
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# 端到端自检：建会话 → 授权 → 装脚本 → 录轨迹 → 折叠查询 → 读回校验
cargo run -p pinbridge-cli -- demo

# 读一份轨迹
cargo run -p pinbridge-cli -- replay <FILE.pbt> --head 20

# Python 侧：与 Rust 共享黄金字节的独立性验证
cd python && pip install -e ".[dev]" && pytest -q
```

把 PinBridge 接给 AI 客户端：

```jsonc
// Claude Code / Cursor 的 MCP 配置
{
  "mcpServers": {
    "pinbridge": {
      "command": "pinbridge-mcp",
      "args": ["--trace-dir", "/tmp/pinbridge-traces"]
    }
  }
}
```

## 仓库结构

```text
crates/                 Rust 工作区（9 个 crate，职责单向依赖）
  pinbridge-core/       领域模型：会话/主体/能力/控制权/有界通道
  pinbridge-proto/      线路协议与 MCP 工具描述
  pinbridge-trace/      事件模型 · 重复折叠 · .pbt 编解码
  pinbridge-hub/        控制面：会话 · 脚本生命周期 · 事件窗口 · 溯源
  pinbridge-client/     Hub → agent 客户端（传输抽象 + 回环 TCP）
  pinbridge-sys/        C ABI 类型层与 pinbridge.dll 运行时加载器
  pinbridge-agent/      注入目标进程的 agent（cdylib，导出 C ABI）
  pinbridge-mcp/        MCP stdio 服务（薄工具层）
  pinbridge-cli/        CLI：tools / replay / demo / mcp
native/                 pinbridge.dll（C++ / Pin tool）+ 稳定 C 头文件
python/                 .pbt 离线解码 · 重放 · 污点 · 切片
apps/desktop/           Tauri + React 工作台（人的观察位）
docs/                   架构 · 设计 · 规范 · 开发流程 · ADR
third_party/pin/        Intel Pin SDK 放置位置（不入库）
```

## 三条不可协商的设计规矩

1. **`.pbt` 是唯一真相来源**。Hub 的观察窗口会滚、事件通道会丢，
   只有落盘的轨迹能作为结论依据。所以"要下结论就必须先录轨迹"不是流程洁癖。
2. **丢数据必须被计数并上报**。任何"悄悄丢事件"的实现都视为缺陷——
   AI 把"我看到的"当成"发生过的"，是自动化分析里最危险的一类错觉。
3. **不做静默降级**。未实现的能力一律显式返回错误；轨迹自相矛盾一律拒绝解析。
   一个看起来完整但实际错误的结论，比一个失败要昂贵得多。

## 当前状态

| 里程碑 | 内容 | 状态 |
|--------|------|------|
| **M0** | 工程骨架 · 领域模型 · 协议 · `.pbt` 格式 · MCP 骨架 · 端到端 demo | ✅ 已完成 |
| M1 | Pin C++ 层接通 · agent 内嵌 CPython · 真实内存/断点/单步 | 规划中 |
| M2 | 四类介入时机全部落地 · TF 虚拟化 · 轨迹实时流式回传 | 规划中 |
| M3 | 离线污点/反向切片 · 工作台可视化 | 规划中 |

详见 [docs/roadmap.md](docs/roadmap.md)。M0 的桩函数位置在
[crates/pinbridge-agent/src/exports.rs](crates/pinbridge-agent/src/exports.rs)。

## 文档入口

| 想了解 | 去哪里 |
|--------|--------|
| 全部文档索引 | [docs/README.md](docs/README.md) |
| 架构与组件 | [docs/architecture/overview.md](docs/architecture/overview.md) |
| 控制权/权限到底怎么算 | [docs/architecture/control-ownership.md](docs/architecture/control-ownership.md) |
| `.pbt` 二进制格式 | [docs/specs/pbt-format.md](docs/specs/pbt-format.md) |
| 怎么参与开发 | [CONTRIBUTING.md](CONTRIBUTING.md) · [docs/dev/workflow.md](docs/dev/workflow.md) |
| 为什么这么设计 | [docs/adr/](docs/adr/README.md) |

## 许可

MIT（见 [LICENSE](LICENSE)）。Intel Pin 是第三方组件，按 Intel 自己的许可分发，
不包含在本仓库中，获取方式见 [third_party/pin/README.md](third_party/pin/README.md)。
