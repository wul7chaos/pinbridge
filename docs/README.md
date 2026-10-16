# PinBridge 文档

这份索引按"你要做什么"组织，而不是按目录组织。找不到东西时先看
[glossary.md](glossary.md)（术语表）——本项目有几个词是自造的，
含义必须精确。

## 我第一次接触这个项目

| 想知道 | 读 |
|--------|-----|
| 这是在解决什么问题 | [../README.md](../README.md) |
| 整体架构长什么样 | [architecture/overview.md](architecture/overview.md) |
| 每个 crate 负责什么 | [architecture/components.md](architecture/components.md) |
| 怎么跑起来（不用 Pin） | [dev/getting-started.md](dev/getting-started.md) |
| 有哪些词我不认识 | [glossary.md](glossary.md) |
| 为什么跟别的工具不一样 | [research/inspiration-and-differences.md](research/inspiration-and-differences.md) |

## 我要改代码

| 要动的地方 | 先读 |
|------------|------|
| 权限 / 控制权 / 谁说了算 | [architecture/control-ownership.md](architecture/control-ownership.md) |
| 轨迹格式 / 事件类型 | [specs/pbt-format.md](specs/pbt-format.md) |
| 折叠压缩算法 | [design/trace-compression.md](design/trace-compression.md) |
| 与目标进程的通信 | [specs/ipc-protocol.md](specs/ipc-protocol.md) |
| C++ ↔ Rust 边界 | [specs/c-abi.md](specs/c-abi.md) |
| C++ / Pin 层怎么构建、踩过哪些坑 | [../native/README.md](../native/README.md) |
| 暴露给 AI 的工具 | [specs/mcp-tools.md](specs/mcp-tools.md) |
| 脚本运行时 | [design/script-runtime.md](design/script-runtime.md) |
| 四类介入时机 | [design/injection-points.md](design/injection-points.md) |
| TF / 单步陷阱那类坑 | [design/flags-virtualization.md](design/flags-virtualization.md) |
| 离线分析（污点/切片） | [design/offline-replay.md](design/offline-replay.md) |

## 我要提交 PR

| 步骤 | 读 |
|------|-----|
| 分支怎么开、怎么合 | [dev/workflow.md](dev/workflow.md) |
| 提交信息怎么写 | [dev/commit-convention.md](dev/commit-convention.md) |
| Rust 代码规范 | [dev/coding-standards-rust.md](dev/coding-standards-rust.md) |
| Python 代码规范 | [dev/coding-standards-python.md](dev/coding-standards-python.md) |
| 测试写到什么程度 | [dev/testing.md](dev/testing.md) |
| 能不能加这个依赖 | [dev/dependency-policy.md](dev/dependency-policy.md) |
| 怎么发版本 | [dev/build-and-release.md](dev/build-and-release.md) |
| 顶层约定 | [../CONTRIBUTING.md](../CONTRIBUTING.md) |

## 我要理解"为什么这么设计"

[docs/adr/](adr/README.md) 里有 8 条架构决策记录，每条都写了
**考虑过的替代方案**和**放弃它们的理由**。想知道"为什么不用 Frida / 为什么
不用 Git Flow / 为什么不引 tokio"，答案都在那里。

## 我在做对抗（VMP 等）

| 问题 | 读 |
|------|-----|
| 目标检测到我了，可能在哪一层 | [architecture/security-model.md](architecture/security-model.md) |
| 轨迹怎么压缩 | [design/trace-compression.md](design/trace-compression.md) |
| 怎么找 OEP / Dump 时机 | [design/offline-replay.md](design/offline-replay.md) |
| 目标用 TF 陷阱恶心我 | [design/flags-virtualization.md](design/flags-virtualization.md) |

## 文档自身的规矩

1. **命令必须可执行**。贴进文档的命令要在本机跑通；跑不通的就不要写。
2. **规范与实现分离**。`specs/` 是"必须一致"的部分，`design/` 是"可以讨论"的部分。
3. **每个"不这样做"都要给理由**。只写"我们这样做"的文档，三个月后没人敢改。
4. **中文为主**。这是给中文团队看的工程文档；代码注释与提交信息允许混用英文术语。
