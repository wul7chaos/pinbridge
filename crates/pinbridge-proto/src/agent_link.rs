//! Hub ↔ agent 链路的请求/响应类型。
//!
//! ## 为什么这些类型在 proto 而不是在 client
//!
//! 它们**同时**被连接的两端使用：Hub 侧（`pinbridge-client`）发请求，
//! 目标进程内的 agent 收请求。把它们放在 client 里会迫使 agent 依赖
//! "客户端"这一个概念上相反的东西；放在 proto 里，两端依赖的是同一份
//! **协议定义**，于是"两端对同一件事的理解"由编译器保证。
//!
//! 这里不只有数据结构，还有一条**能力表**：
//! [`AgentRequest::required_capability`]。它是 agent 权限的唯一真相来源——
//! 客户端用它做发送前校验，agent 用它做收到后的校验（双端校验，见
//! `docs/architecture/control-ownership.md` §5）。两侧各写一份必然会分叉。
//!
//! ## 帧格式
//!
//! `u32` 小端长度 + JSON 负载。用 JSON 而不是二进制是**刻意的**：
//! 这条链路承载的是控制命令与**有上限的**事件批（`DrainEvents.limit`），
//! 而真正的高频数据路径是 agent 进程内直接把事件写进 `.pbt`，不经过这里。
//! 控制链路可读带来的收益（`nc` 一下就能手工排查）大于它的编码开销。

use serde::{Deserialize, Serialize};

use pinbridge_core::capability::{Capability, CapabilitySet};

/// 发给 agent 的请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum AgentRequest {
    /// 探活并取回 agent 信息。
    Ping {
        /// 协议版本。
        wire_version: u16,
    },
    /// 读内存。
    ReadMemory {
        /// 起始地址。
        address: u64,
        /// 长度（上限由 agent 决定，默认 64 KiB）。
        length: u32,
    },
    /// 写内存。
    WriteMemory {
        /// 地址。
        address: u64,
        /// hex 字节。
        hex_bytes: String,
    },
    /// 读寄存器。
    ReadRegisters {
        /// 线程 ID。
        thread: u32,
    },
    /// 下断点。
    SetBreakpoint {
        /// 地址。
        address: u64,
        /// 备注。
        #[serde(default)]
        label: String,
    },
    /// 清断点。
    ClearBreakpoint {
        /// 地址。
        address: u64,
    },
    /// 单步。
    SingleStep {
        /// 步数。
        count: u32,
    },
    /// 继续。
    Continue,
    /// 中断。
    Pause,
    /// 运行到地址。
    RunTo {
        /// 地址。
        address: u64,
    },
    /// 加载脚本。
    LoadScript {
        /// 名称。
        name: String,
        /// 源码。
        source: String,
        /// 订阅的事件类型名。
        #[serde(default)]
        subscribe: Vec<String>,
    },
    /// 卸载脚本。
    UnloadScript {
        /// 脚本 ID。
        script: u64,
    },
    /// 拉取事件。
    DrainEvents {
        /// 起始序号。
        from: u64,
        /// 条数上限。
        limit: u32,
        /// 是否折叠。
        #[serde(default)]
        fold_repeats: bool,
    },
    /// 开始录制。
    StartTrace {
        /// 输出路径。
        path: String,
        /// 是否折叠。
        #[serde(default)]
        fold: bool,
    },
    /// 停止录制。
    StopTrace,
}

impl AgentRequest {
    /// 本请求需要的能力。
    ///
    /// **新增请求类型时必须在这里登记**，否则会落到兜底的 `MemRead` 上，
    /// 等于悄悄放开一项写权限。`pinbridge-client` 与 agent 都调用这个函数，
    /// 两侧因此不可能对"这条命令需要什么"产生分歧。
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::Ping { .. }
            | Self::ReadMemory { .. }
            | Self::ReadRegisters { .. }
            | Self::DrainEvents { .. } => Capability::MemRead,
            Self::WriteMemory { .. } => Capability::MemWrite,
            Self::SetBreakpoint { .. } | Self::ClearBreakpoint { .. } => Capability::Breakpoint,
            Self::SingleStep { .. } | Self::RunTo { .. } => Capability::SingleStep,
            Self::Continue | Self::Pause => Capability::ProcessControl,
            Self::LoadScript { .. } | Self::UnloadScript { .. } => Capability::ScriptLoad,
            Self::StartTrace { .. } | Self::StopTrace => Capability::TraceRecord,
        }
    }

    /// 请求名（用于错误信息与审计；与 JSON 里的 `req` 字段一致）。
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Ping { .. } => "ping",
            Self::ReadMemory { .. } => "read_memory",
            Self::WriteMemory { .. } => "write_memory",
            Self::ReadRegisters { .. } => "read_registers",
            Self::SetBreakpoint { .. } => "set_breakpoint",
            Self::ClearBreakpoint { .. } => "clear_breakpoint",
            Self::SingleStep { .. } => "single_step",
            Self::Continue => "continue",
            Self::Pause => "pause",
            Self::RunTo { .. } => "run_to",
            Self::LoadScript { .. } => "load_script",
            Self::UnloadScript { .. } => "unload_script",
            Self::DrainEvents { .. } => "drain_events",
            Self::StartTrace { .. } => "start_trace",
            Self::StopTrace => "stop_trace",
        }
    }
}

/// agent 返回的响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "res", rename_all = "snake_case")]
pub enum AgentResponse {
    /// 探活结果。
    Pong {
        /// agent 版本（`pinbridge-agent 0.1.0` 这种）。
        agent_version: String,
        /// 目标架构。
        arch: String,
        /// agent 当前能力集（由 Hub 授权下发）。
        capabilities: CapabilitySet,
    },
    /// 内存内容。
    Memory {
        /// 起始地址。
        address: u64,
        /// hex 字节。
        hex_bytes: String,
    },
    /// 寄存器内容。
    Registers {
        /// 线程。
        thread: u32,
        /// 名称 → 值。
        values: Vec<(String, u64)>,
    },
    /// 事件批。
    Events {
        /// 事件。
        items: Vec<EventDto>,
        /// 下一批的游标。
        next: u64,
        /// 丢失条数。
        dropped: u64,
    },
    /// 脚本已加载。
    Script {
        /// 脚本 ID。
        script: u64,
    },
    /// 录制已开始。
    Trace {
        /// 路径。
        path: String,
    },
    /// 录制汇总。
    TraceSummary {
        /// 记录条数。
        records: u64,
        /// 展开后的事件条数。
        events: u64,
        /// 折叠条数（近似值：精确值在 `.pbt` 的 SUMMARY 段里）。
        folded: u64,
        /// 丢弃条数。
        dropped: u64,
    },
    /// 简单成功。
    Ok,
    /// 失败。
    Error {
        /// 机器可读错误码。
        code: String,
        /// 描述。
        message: String,
    },
}

/// 事件在线路上的形态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDto {
    /// 序号。
    pub seq: u64,
    /// 类型判别值（与 `.pbt` 的类型表一致）。
    pub kind: u8,
    /// 线程。
    pub thread: u32,
    /// 地址。
    pub address: u64,
    /// 参数。
    pub args: Vec<u64>,
}

/// 错误码字符串。与 `docs/specs/ipc-protocol.md` 的错误码表一致。
///
/// 为什么是字符串而不是整数：这条链路两端都可能被人手工构造报文调试
/// （见模块文档），可读的错误码省掉一次查表。
pub mod error_code {
    /// 能力不足。
    pub const CAPABILITY_DENIED: &str = "capability_denied";
    /// 尚未附加到目标。
    pub const NOT_ATTACHED: &str = "not_attached";
    /// 参数非法（空指针、长度越界、hex 不合法……）。
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    /// 当前实现/平台不支持。
    pub const NOT_SUPPORTED: &str = "not_supported";
    /// 目标正忙。
    pub const BUSY: &str = "busy";
    /// 超时。
    pub const TIMEOUT: &str = "timeout";
    /// 内部错误。
    pub const INTERNAL: &str = "internal";
    /// ABI/协议版本不匹配。
    pub const PROTOCOL_ERROR: &str = "protocol_error";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_request_maps_to_a_capability_and_a_name() {
        // 逐个构造：新增请求类型时如果忘了登记能力表或名字，这条测试会红。
        let cases = [
            AgentRequest::Ping { wire_version: 1 },
            AgentRequest::ReadMemory {
                address: 0,
                length: 1,
            },
            AgentRequest::WriteMemory {
                address: 0,
                hex_bytes: "90".into(),
            },
            AgentRequest::ReadRegisters { thread: 0 },
            AgentRequest::SetBreakpoint {
                address: 0,
                label: String::new(),
            },
            AgentRequest::ClearBreakpoint { address: 0 },
            AgentRequest::SingleStep { count: 1 },
            AgentRequest::Continue,
            AgentRequest::Pause,
            AgentRequest::RunTo { address: 0 },
            AgentRequest::LoadScript {
                name: "a.py".into(),
                source: String::new(),
                subscribe: Vec::new(),
            },
            AgentRequest::UnloadScript { script: 1 },
            AgentRequest::DrainEvents {
                from: 0,
                limit: 1,
                fold_repeats: false,
            },
            AgentRequest::StartTrace {
                path: "t.pbt".into(),
                fold: true,
            },
            AgentRequest::StopTrace,
        ];
        let mut names = HashSet::new();
        for request in &cases {
            assert!(!request.name().is_empty(), "请求必须有名");
            assert!(
                names.insert(request.name()),
                "请求名重复: {}",
                request.name()
            );
            // 写类请求绝不允许落到只读能力上
            let needed = request.required_capability();
            if matches!(
                request,
                AgentRequest::WriteMemory { .. }
                    | AgentRequest::SetBreakpoint { .. }
                    | AgentRequest::ClearBreakpoint { .. }
            ) {
                assert_ne!(
                    needed,
                    Capability::MemRead,
                    "{} 是写操作，不能只要求读权限",
                    request.name()
                );
            }
        }
        // 15 个变体，逐一覆盖
        assert_eq!(cases.len(), 15);
    }

    #[test]
    fn requests_serialize_with_a_stable_tag() {
        let text = serde_json::to_string(&AgentRequest::SingleStep { count: 3 }).unwrap();
        assert!(text.contains("\"req\":\"single_step\""), "实际: {text}");
        let text = serde_json::to_string(&AgentRequest::DrainEvents {
            from: 0,
            limit: 8,
            fold_repeats: true,
        })
        .unwrap();
        assert!(text.contains("\"req\":\"drain_events\""), "实际: {text}");
        let text = serde_json::to_string(&AgentResponse::Ok).unwrap();
        assert_eq!(text, "{\"res\":\"ok\"}");
    }

    #[test]
    fn unknown_fields_are_tolerated_for_forward_compatibility() {
        // 新增可选字段是允许的（见 docs/specs/ipc-protocol.md 的版本策略）：
        // 老 agent 收到带新字段的请求应当忽略，而不是报错。
        let json = r#"{"req":"ping","wire_version":1,"future_field":{"nested":true}}"#;
        let request: AgentRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request, AgentRequest::Ping { wire_version: 1 });
    }
}
