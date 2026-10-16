//! Hub → agent 的客户端。
//!
//! ## 链路
//!
//! ```text
//! HubService ── AgentClient ──▶ Transport ──▶ pinbridge_agentdll（目标进程内）
//!                                   │
//!                     TCP 回环（M1 默认） / 内存管道（测试）
//! ```
//!
//! ## 为什么把 `Transport` 抽象出来
//!
//! agent 必须能在"目标进程被暂停在任意指令上"时被调用，这意味着任何一次
//! 往返都可能因为目标异常而中断。把传输层抽出来之后：
//!
//! * 单元测试用 [`MemoryTransport`] 在毫秒级完成，不依赖 Pin 与目标进程；
//! * 真实链路换成回环 TCP 时，上层逻辑（超时、重试、错误映射）不需要改；
//! * 未来要支持"agent 在另一台机器的容器里"时，只加一个 `SshTransport`。
//!
//! 回环 TCP 的**绑定策略是安全边界**：只允许 `127.0.0.1`，端口由 agent 启动时
//! 上报，不接受外部传入的地址。这一条不是可配置项——一个能操控目标进程内存的
//! 端口一旦监听在外部接口上，就等于把调试能力敞开给整个网络。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod transport;

pub use client::{AgentClient, AgentRequest, AgentResponse, ClientError};
pub use transport::{MemoryTransport, Transport};
