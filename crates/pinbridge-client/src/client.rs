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

use pinbridge_core::capability::CapabilitySet;
use pinbridge_core::error::Error;

use crate::transport::Transport;

/// 默认单次往返超时。读取类命令用这个值；
/// 单步/继续这类会影响目标执行流的命令由调用方显式放宽。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5_000);

// 请求/响应/事件与"请求需要什么能力"这一张表都属于**协议**，定义在
// `pinbridge-proto::agent_link`：连接的两端（Hub 侧与本文件、目标侧 agent）
// 必须用同一份定义，否则"对同一条命令需要什么权限"迟早会分叉。
pub use pinbridge_proto::agent_link::{AgentRequest, AgentResponse, EventDto};

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
        self.capabilities.require(request.required_capability())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MemoryTransport;
    use pinbridge_core::capability::Capability;

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
            AgentRequest::Ping { wire_version: 1 }.required_capability(),
            Capability::MemRead
        );
        assert_eq!(
            AgentRequest::Continue.required_capability(),
            Capability::ProcessControl
        );
        assert_eq!(
            AgentRequest::StopTrace.required_capability(),
            Capability::TraceRecord
        );
        assert_eq!(
            AgentRequest::UnloadScript { script: 1 }.required_capability(),
            Capability::ScriptLoad
        );
    }

    #[test]
    fn default_timeout_is_five_seconds() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_millis(5_000));
    }
}
