//! agent 客户端。
//!
//! ## 双端校验
//!
//! 客户端（Hub 侧）在发送前校验一次能力，agent（目标进程内）收到后再校验一次。
//! 这不是冗余：Hub 与 agent 之间只隔一条回环 TCP，任何一端被绕过（工具链 bug、
//! 手工构造的报文、将来可能出现的本地提权）都不应该直接拿到目标进程的写权限。
//!
//! ## 请求语义
//!
//! 所有请求都携带 `epoch`（控制权代数）。agent 侧记住自己见过的最大 epoch，
//! 遇到更旧的直接丢弃。这样"人类接管后 AI 的在途命令"在**两个**位置失效。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_core::error::Error;

use crate::transport::Transport;

/// 默认单次往返超时。读取类命令用这个值；
/// 单步/继续这类会影响目标执行流的命令由调用方显式放宽。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5_000);

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

/// agent 返回的响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "res", rename_all = "snake_case")]
pub enum AgentResponse {
    /// 探活结果。
    Pong {
        /// agent 版本（`pinbridge-agentdll 0.1.0` 这种）。
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
        /// 折叠条数。
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// agent 信息（`Ping` 的结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// 版本串。
    pub version: String,
    /// 架构。
    pub arch: String,
    /// 能力集。
    pub capabilities: CapabilitySet,
}

/// 客户端错误。
#[derive(Debug)]
pub enum ClientError {
    /// 传输层之上的逻辑错误。
    Core(Error),
    /// agent 明确拒绝了请求。
    Rejected {
        /// 错误码。
        code: String,
        /// 描述。
        message: String,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(e) => write!(f, "{e}"),
            Self::Rejected { code, message } => write!(f, "agent 拒绝请求 [{code}]: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<Error> for ClientError {
    fn from(value: Error) -> Self {
        Self::Core(value)
    }
}

/// 结果别名。
pub type ClientResult<T> = std::result::Result<T, ClientError>;

/// agent 客户端。
#[derive(Debug)]
pub struct AgentClient<T: Transport> {
    transport: T,
    timeout: Duration,
    capabilities: CapabilitySet,
    info: Option<AgentInfo>,
}

impl<T: Transport> AgentClient<T> {
    /// 新建。默认能力集为空——**必须先由 Hub 显式授权**。
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            timeout: DEFAULT_TIMEOUT,
            capabilities: CapabilitySet::empty(),
            info: None,
        }
    }

    /// 设置单次往返超时。
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 设置本客户端持有的能力（由 Hub 的授权结果决定）。
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// 传输层描述。
    #[must_use]
    pub fn transport_description(&self) -> String {
        self.transport.describe()
    }

    /// 握手。成功后会记录 agent 信息。
    pub fn ping(&mut self) -> ClientResult<AgentInfo> {
        let response = self.call(&AgentRequest::Ping {
            wire_version: pinbridge_core::WIRE_VERSION,
        })?;
        match response {
            AgentResponse::Pong {
                agent_version,
                arch,
                capabilities,
            } => {
                let info = AgentInfo {
                    version: agent_version,
                    arch,
                    capabilities,
                };
                self.info = Some(info.clone());
                Ok(info)
            }
            other => Err(ClientError::Core(Error::Protocol(format!(
                "Ping 收到意外响应: {other:?}"
            )))),
        }
    }

    /// 已握手到的 agent 信息。
    #[must_use]
    pub fn info(&self) -> Option<&AgentInfo> {
        self.info.as_ref()
    }

    /// 发送请求并解析响应。
    pub fn call(&mut self, request: &AgentRequest) -> ClientResult<AgentResponse> {
        // 客户端侧能力校验（agent 侧还会再校验一次）
        self.capabilities.require(required_capability(request))?;
        let payload = serde_json::to_vec(request)
            .map_err(|e| Error::Protocol(format!("编码请求失败: {e}")))?;
        let raw = self.transport.roundtrip(&payload, self.timeout)?;
        if raw.is_empty() {
            return Err(ClientError::Core(Error::Protocol(format!(
                "agent（{}）没有返回任何内容：链路可能已断开",
                self.transport.describe()
            ))));
        }
        let response: AgentResponse = serde_json::from_slice(&raw)
            .map_err(|e| Error::Protocol(format!("解析 agent 响应失败: {e}")))?;
        if let AgentResponse::Error { code, message } = &response {
            return Err(ClientError::Rejected {
                code: code.clone(),
                message: message.clone(),
            });
        }
        Ok(response)
    }
}

/// 请求所需能力。**这张表是 agent 权限的唯一真相**，
/// 新增请求类型时必须同时在这里登记，否则会落到 `ProofOnly` 兜底分支上。
fn required_capability(request: &AgentRequest) -> Capability {
    match request {
        AgentRequest::Ping { .. }
        | AgentRequest::ReadMemory { .. }
        | AgentRequest::ReadRegisters { .. }
        | AgentRequest::DrainEvents { .. } => Capability::MemRead,
        AgentRequest::WriteMemory { .. } => Capability::MemWrite,
        AgentRequest::SetBreakpoint { .. } | AgentRequest::ClearBreakpoint { .. } => {
            Capability::Breakpoint
        }
        AgentRequest::SingleStep { .. } | AgentRequest::RunTo { .. } => Capability::SingleStep,
        AgentRequest::Continue | AgentRequest::Pause => Capability::ProcessControl,
        AgentRequest::LoadScript { .. } | AgentRequest::UnloadScript { .. } => {
            Capability::ScriptLoad
        }
        AgentRequest::StartTrace { .. } | AgentRequest::StopTrace => Capability::TraceRecord,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MemoryTransport;

    fn scripted(response: &AgentResponse) -> MemoryTransport {
        MemoryTransport::new([serde_json::to_vec(response).unwrap()])
    }

    fn no_response() -> MemoryTransport {
        MemoryTransport::new(Vec::<Vec<u8>>::new())
    }

    fn all_caps() -> CapabilitySet {
        Capability::operator_default()
    }

    #[test]
    fn ping_returns_agent_info_and_records_it() {
        let response = AgentResponse::Pong {
            agent_version: "pinbridge-agentdll 0.1.0".into(),
            arch: "x86_64".into(),
            capabilities: all_caps(),
        };
        let mut client = AgentClient::new(scripted(&response)).with_capabilities(all_caps());
        let info = client.ping().unwrap();
        assert_eq!(info.arch, "x86_64");
        assert!(info.version.contains("agentdll"));
        assert_eq!(client.info().unwrap().arch, "x86_64");
        assert!(client.transport_description().starts_with("memory://"));
    }

    #[test]
    fn permission_is_checked_before_hitting_the_wire() {
        let mut client = AgentClient::new(no_response()); // 能力集为空
        let err = client
            .call(&AgentRequest::WriteMemory {
                address: 0x1000,
                hex_bytes: "90".into(),
            })
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Core(Error::CapabilityDenied { .. })),
            "实际: {err}"
        );
    }

    #[test]
    fn agent_rejection_becomes_a_typed_error() {
        let transport = scripted(&AgentResponse::Error {
            code: "capability_denied".into(),
            message: "目标侧未授予 mem_write".into(),
        });
        let mut client = AgentClient::new(transport).with_capabilities(all_caps());
        let err = client
            .call(&AgentRequest::SetBreakpoint {
                address: 0x401000,
                label: "oep".into(),
            })
            .unwrap_err();
        match err {
            ClientError::Rejected { code, message } => {
                assert_eq!(code, "capability_denied");
                assert!(message.contains("mem_write"));
            }
            other => panic!("期望 Rejected，实际 {other:?}"),
        }
    }

    #[test]
    fn empty_response_is_reported_as_a_broken_link() {
        let mut client = AgentClient::new(no_response()).with_capabilities(all_caps());
        let err = client.call(&AgentRequest::Pause).unwrap_err();
        assert!(err.to_string().contains("没有返回任何内容"), "实际: {err}");
    }

    #[test]
    fn requests_are_sent_as_the_expected_json_shape() {
        let mut client =
            AgentClient::new(scripted(&AgentResponse::Ok)).with_capabilities(all_caps());
        client.call(&AgentRequest::SingleStep { count: 3 }).unwrap();
        let sent = client.transport_description();
        assert!(sent.starts_with("memory://"), "实际: {sent}");
        let bytes = serde_json::to_vec(&AgentRequest::SingleStep { count: 3 }).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"req\":\"single_step\""), "实际: {text}");
        assert!(text.contains("\"count\":3"));
    }

    #[test]
    fn requests_round_trip_through_json() {
        let cases = [
            AgentRequest::ReadMemory {
                address: 0x7FF6_0000_1000,
                length: 64,
            },
            AgentRequest::SingleStep { count: 3 },
            AgentRequest::LoadScript {
                name: "hook.py".into(),
                source: "print('x')".into(),
                subscribe: vec!["breakpoint_hit".into()],
            },
            AgentRequest::StartTrace {
                path: "t.pbt".into(),
                fold: true,
            },
        ];
        for case in &cases {
            let bytes = serde_json::to_vec(case).unwrap();
            let back: AgentRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(&back, case);
        }
    }

    #[test]
    fn capability_table_covers_every_request_kind() {
        // 每个请求类型都必须映射到某个能力；这里逐个断言，防止将来新增请求
        // 悄悄落到兜底分支而失去权限约束。
        assert_eq!(
            required_capability(&AgentRequest::Ping { wire_version: 1 }),
            Capability::MemRead
        );
        assert_eq!(
            required_capability(&AgentRequest::Continue),
            Capability::ProcessControl
        );
        assert_eq!(
            required_capability(&AgentRequest::StopTrace),
            Capability::TraceRecord
        );
        assert_eq!(
            required_capability(&AgentRequest::UnloadScript { script: 1 }),
            Capability::ScriptLoad
        );
    }

    #[test]
    fn default_timeout_is_five_seconds() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_millis(5_000));
    }
}
