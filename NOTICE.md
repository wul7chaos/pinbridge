# 使用边界与第三方声明

本文件是**说明**，不是许可条款。许可条款以 [LICENSE](LICENSE)（MIT）为准。

## 使用边界

PinBridge 是一个**面向授权的动态分析框架**：它基于插装技术观察并干预
**用户态目标进程**的行为，供安全研究员、逆向工程师与 AI agent
在**自己的样本、自己的机器、自己的授权范围**内做动静结合的分析。

请只对**你拥有或已获授权**的目标使用它。
使用者的行为由使用者负责；维护者不会协助、也不会讨论对未授权目标的分析。

## Intel Pin

PinBridge 以 **Intel Pin** 作为插装基座（另一个可选基座是 DynamoRIO）。

* Intel Pin **按 Intel 自己的许可分发**，不包含在本仓库中；
* 本仓库的 `.gitignore` 排除了 `third_party/pin/`（只保留说明文件）；
* 获取方式与版本记录见 [third_party/pin/README.md](third_party/pin/README.md)；
* 使用 Pin 的条款由 Intel 规定，请自行确认你的使用场景符合它。

Rust/Python/C++ 的**本项目代码**全部是本仓库原创（设计灵感来源见
[docs/research/inspiration-and-differences.md](docs/research/inspiration-and-differences.md)），
不包含、也不声称包含任何第三方的专有代码。

## 第三方依赖

Rust 侧运行期依赖：`serde` / `serde_json`（MIT OR Apache-2.0）、
`thiserror`（MIT OR Apache-2.0）、`sha2`（MIT OR Apache-2.0）。
Python 侧运行期依赖：无。

依赖的许可证与已知公告由 CI 的 `cargo deny` 检查，
策略见 [deny.toml](deny.toml) 与 [docs/dev/dependency-policy.md](docs/dev/dependency-policy.md)。

## 不提供的东西

为了让边界清晰，这里明确列出本项目**不提供**的能力：

* 不提供、不内置、不协助获取任何恶意样本或授权材料；
* 不提供绕过软件授权、许可验证或保护机制的能力；
* 不提供远程控制面（agent 链路只允许本机回环地址）；
* 不提供自更新或代码下传机制。
