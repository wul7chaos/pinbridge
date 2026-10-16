//! PinBridge 线路协议（wire protocol）。
//!
//! 三层链路各有自己的编解码器，**只有本 crate 定义它们的公共部分**：
//!
//! ```text
//!   AI 客户端 ── MCP stdio (JSON-RPC 2.0) ──▶ pinbridge-mcp ─┐
//!   人类/工作台 ── Tauri invoke / CLI ─────────────────────────┤──▶ HubService
//!                                                             │
//!   HubService ── 本机回环 TCP，u32 长度前缀 + JSON ──▶ agent(pinbridge_agent)
//! ```
//!
//! ### 为什么 agent 链路也用 JSON
//!
//! 早期的设计笔记写的是"二进制 + 压缩"，实现时改成了 JSON，理由要写清楚：
//! 这条链路承载的是**控制命令**与**有上限的事件批**（`DrainEvents.limit`），
//! 而真正的高频数据路径是 agent 在目标进程内**直接把事件写进 `.pbt`**，
//! 根本不经过这里。于是可读性的收益（`nc` 一下就能手工排查链路问题）
//! 大过 JSON 的编码开销。
//!
//! 什么时候该改回二进制：当"边录边看"需要把每秒十万级的原始事件在线路上
//! 传出去时（M2 的流式回传）。那时应当新增一种**二进制帧**而不是替换它，
//! 让控制链路保持可以手工对话。
//!
//! 版本策略：任何字段的删除/改名都必须提升 [`pinbridge_core::WIRE_VERSION`]，
//! 并同步 `docs/specs/ipc-protocol.md`。新增可选字段则不需要。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent_link;
pub mod codec;
pub mod frames;
pub mod mcp;

pub use agent_link::{AgentRequest, AgentResponse, EventDto};
pub use codec::{decode_line, encode_line, MAX_FRAME_BYTES};
pub use frames::{ControlCommand, ErrorPayload, Request, Response, ResponseBody};
