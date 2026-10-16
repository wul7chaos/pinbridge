//! PinBridge 线路协议（wire protocol）。
//!
//! 三层链路各有自己的编解码器，**只有本 crate 定义它们的公共部分**：
//!
//! ```text
//!   AI 客户端 ── MCP stdio (JSON-RPC 2.0) ──▶ pinbridge-mcp ─┐
//!   人类/工作台 ── Tauri invoke / CLI ─────────────────────────┤──▶ HubService
//!                                                             │
//!   HubService ── 本机二进制 TCP，长度前缀 + 压缩 ──▶ agent(pinbridge_agentdll)
//! ```
//!
//! 为什么不让 agent 也说 JSON：事件通道的量级是每秒百万条，JSON 的编码开销会
//! 直接吃掉目标进程的 CPU。所以 agent 链路走二进制（见 `pinbridge_trace::pbt`
//! 的编码原语），Hub 以上走 JSON——**人看得懂**在调试期是硬需求。
//!
//! 版本策略：任何字段的删除/改名都必须提升 [`pinbridge_core::WIRE_VERSION`]，
//! 并同步 `docs/specs/ipc-protocol.md`。新增可选字段则不需要。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codec;
pub mod frames;
pub mod mcp;

pub use codec::{decode_line, encode_line, MAX_FRAME_BYTES};
pub use frames::{ControlCommand, ErrorPayload, Request, Response, ResponseBody};
