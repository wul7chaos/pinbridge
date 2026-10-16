//! `pinbridge-mcp`：把控制面暴露给 AI 客户端的 MCP 服务。
//!
//! # 传输
//!
//! `stdio`（MCP 的标准本地传输）。选择理由与 [包设计一致](../docs/architecture/components.md)：
//! AI 客户端（Claude Code / Cursor / 自研 agent）几乎都支持 stdio，
//! 而 stdio 天然不暴露网络端口——对一个能改目标进程内存的工具，这是最重要的一条。
//!
//! # 与 Hub 的关系
//!
//! 这一层**不持有分析逻辑**，它只做三件事：
//!
//! 1. 把 MCP 的 JSON-RPC 翻译成 `HubService` 调用；
//! 2. 把错误翻译成 AI 能据以行动的 `tool_result`（含 `hint`）；
//! 3. 维护每个会话的 **epoch 缓存**，让"人类接管后 AI 的在途命令失效"这条语义
//!    在 MCP 这一层也成立。
//!
//! 第 3 点容易被忽略：如果服务端每次都从 Hub 取最新 epoch，epoch 校验就成了
//! 空转（永远相等）。所以这里缓存 attach 时拿到的 epoch，只有响应里明确带回
//! 新 epoch 时才刷新。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod link;
pub mod server;

pub use server::{serve_stdio, McpServer, ProtocolError};
