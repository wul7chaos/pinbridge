//! 请求 / 响应帧定义。
//!
//! 命名约定：`snake_case` 的 `op` 标签 + 扁平字段。刻意不使用嵌套的
//! `{"method": "...", "params": {...}}`：AI 生成工具调用时字段越浅，
//! 出错概率越低；而人工用 curl/nc 手打时也不容易漏括号。

use serde::{Deserialize, Serialize};

use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_core::id::{EventSeq, ProcessId};
use pinbridge_core::session::SessionState;

/// 控制类命令。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlCommand {
    /// 继续运行。
    Continue,
    /// 中断目标（异步生效，响应只表示"已下发"）。
    Pause,
    /// 单步（`count` 条指令，默认 1）。
    Step {
        /// 步数。
        #[serde(default = "one")]
        count: u32,
    },
    /// 运行到地址。
    RunTo {
        /// 目标地址（目标进程虚拟地址）。
        address: u64,
    },
    /// 设置断点。
    BreakpointSet {
        /// 地址。
        address: u64,
        /// 备注（会出现在事件流与审计里）。
        #[serde(default)]
        label: String,
    },
    /// 清除断点。
    BreakpointClear {
        /// 地址。
        address: u64,
    },
    /// 设置指令模板匹配 Hook。
    PatternHook {
        /// 字节模板，如 `"8B ?? ?? ?? ?? 85 C0"`。
        pattern: String,
        /// 备注。
        #[serde(default)]
        label: String,
    },
}

fn one() -> u32 {
    1
}

/// 请求。每条请求都必须携带 `epoch`，Hub 用它做控制权校验。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 握手：声明主体身份与协议版本。
    Hello {
        /// 协议版本（应等于 `WIRE_VERSION`）。
        wire_version: u16,
        /// 主体类型：`human` / `ai`。
        principal_kind: String,
        /// 主体名称。
        principal_name: String,
    },
    /// 列出会话。
    ListSessions,
    /// 新建会话（启动目标）。
    CreateSession {
        /// 目标可执行文件路径。
        image_path: String,
        /// 架构。
        arch: String,
        /// 命令行参数。
        #[serde(default)]
        args: Vec<String>,
    },
    /// 附加到已运行进程。
    Attach {
        /// PID。
        pid: u32,
    },
    /// 断开并保留会话记录。
    Detach {
        /// 会话 ID。
        session: u64,
    },
    /// 控制类命令。
    Control {
        /// 会话 ID。
        session: u64,
        /// 控制权 epoch。
        epoch: u64,
        /// 具体命令。
        command: ControlCommand,
    },
    /// 加载脚本。
    LoadScript {
        /// 会话 ID。
        session: u64,
        /// epoch。
        epoch: u64,
        /// 脚本名（用于审计与替换同源脚本）。
        name: String,
        /// 脚本正文（Python）。
        source: String,
        /// 订阅的事件类型。
        #[serde(default)]
        subscribe: Vec<String>,
    },
    /// 卸载脚本。
    UnloadScript {
        /// 会话 ID。
        session: u64,
        /// epoch。
        epoch: u64,
        /// 脚本 ID。
        script: u64,
    },
    /// 读内存。
    ReadMemory {
        /// 会话 ID。
        session: u64,
        /// 起始地址。
        address: u64,
        /// 长度。
        length: u32,
    },
    /// 写内存。
    WriteMemory {
        /// 会话 ID。
        session: u64,
        /// epoch。
        epoch: u64,
        /// 地址。
        address: u64,
        /// hex 编码的字节。
        hex_bytes: String,
    },
    /// 读寄存器。
    ReadRegisters {
        /// 会话 ID。
        session: u64,
        /// 线程 ID。
        thread: u32,
    },
    /// 授予 / 收窄能力集（仅人类，需要 `CapabilityGrant`）。
    GrantCapabilities {
        /// 会话 ID。
        session: u64,
        /// epoch。
        epoch: u64,
        /// 被授权主体名。
        target_principal: String,
        /// 授予的能力列表。
        capabilities: Vec<Capability>,
    },
    /// 开始录制轨迹。
    StartTrace {
        /// 会话 ID。
        session: u64,
        /// epoch。
        epoch: u64,
        /// 输出路径（Hub 侧决定）。
        path: String,
        /// 压缩档位：`none` / `repeat` / `repeat+delta`。
        #[serde(default)]
        profile: String,
    },
    /// 停止录制。
    StopTrace {
        /// 会话 ID。
        session: u64,
        /// epoch。
        epoch: u64,
    },
    /// 拉取聚合后的事件（AI 的主要观察手段）。
    QueryEvents {
        /// 会话 ID。
        session: u64,
        /// 起始序号（含）。
        from: u64,
        /// 最多返回多少条。
        #[serde(default = "default_limit")]
        limit: u32,
        /// 是否合并重复事件。
        #[serde(default = "default_true")]
        fold_repeats: bool,
    },
    /// 查看审计/溯源记录。
    Provenance {
        /// 会话 ID。
        session: u64,
    },
    /// 优雅关闭。
    Shutdown,
}

fn default_limit() -> u32 {
    256
}
fn default_true() -> bool {
    true
}

/// 错误载荷。`hint` 是给 AI 的"下一步怎么办"，不是给人类看的排查建议。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorPayload {
    /// 机器可读的错误码。
    pub code: String,
    /// 人类可读描述。
    pub message: String,
    /// 给调用方的可执行建议（AI 会直接读这个字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// 成功响应的载荷。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseBody {
    /// 握手结果：返回分配的 epoch 与能力集。
    Hello {
        /// 服务端协议版本。
        wire_version: u16,
        /// 本会话的 epoch。
        epoch: u64,
        /// 授予调用方的主体名。
        principal: String,
        /// 当前能力集。
        capabilities: CapabilitySet,
    },
    /// 会话列表。
    Sessions {
        /// 每条会话的摘要。
        items: Vec<SessionSummary>,
    },
    /// 会话详情。
    Session {
        /// 摘要。
        summary: SessionSummary,
    },
    /// 控制命令已受理。
    Accepted {
        /// 受理时的 epoch（用于后续对账）。
        epoch: u64,
    },
    /// 脚本已加载。
    ScriptLoaded {
        /// 脚本 ID。
        script: u64,
        /// 源码哈希（sha256 hex），溯源用。
        source_sha256: String,
    },
    /// 读取的内存（hex）。
    Memory {
        /// 起始地址。
        address: u64,
        /// hex 字节。
        hex_bytes: String,
    },
    /// 寄存器快照。
    Registers {
        /// 线程 ID。
        thread: u32,
        /// 名称 → 值。
        values: Vec<(String, u64)>,
    },
    /// 事件批。
    Events {
        /// 事件（已按请求折算重复）。
        items: Vec<EventView>,
        /// 本批之后的建议游标。
        next: u64,
        /// 因通道背压被丢弃的事件数（必须上报）。
        dropped: u64,
    },
    /// 溯源记录。
    Provenance {
        /// 记录条目（JSON 文本，顺序即发生顺序）。
        entries: Vec<String>,
    },
    /// 简单确认。
    Ok,
}

/// 会话摘要（列表用，字段少但足够定位）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 会话 ID。
    pub session: u64,
    /// PID。
    pub pid: ProcessId,
    /// 状态。
    pub state: SessionState,
    /// 目标路径。
    pub image_path: String,
    /// 当前控制权持有者（`ai:agent-7` 这种审计名）。
    pub lease_holder: String,
    /// 已加载脚本数。
    pub scripts: usize,
}

/// 事件的人类可读视图（AI 直接消费）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventView {
    /// 序号。
    pub seq: EventSeq,
    /// 事件类型名。
    pub kind: String,
    /// 线程。
    pub thread: u32,
    /// 地址。
    pub address: u64,
    /// 参数。
    pub args: Vec<u64>,
    /// 折叠出来的重复次数（非折叠事件为 1）。
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub repeats: u32,
}

fn is_one(v: &u32) -> bool {
    *v == 1
}

/// 响应。与请求一一对应（`id` 用于并发对账）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// 对应请求的 id。
    pub id: u64,
    /// 结果。
    #[serde(flatten)]
    pub body: ResponseBody,
}

/// 能力集的 JSON 表示由 `pinbridge-core` 直接提供
/// （`{"bits": N, "names": [...]}`），这里不再包一层，避免出现
/// "协议里一种写法、文档里另一种写法"的分裂。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape_is_flat() {
        let req = Request::Control {
            session: 1,
            epoch: 2,
            command: ControlCommand::Step { count: 3 },
        };
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.contains("\"op\":\"control\""), "实际: {text}");
        assert!(text.contains("\"cmd\":\"step\""));
        assert!(text.contains("\"count\":3"));
    }

    #[test]
    fn step_defaults_to_one() {
        let cmd: ControlCommand = serde_json::from_str("{\"cmd\":\"step\"}").unwrap();
        assert_eq!(cmd, ControlCommand::Step { count: 1 });
    }

    #[test]
    fn event_view_omits_single_repeat() {
        let view = EventView {
            seq: EventSeq::new(9),
            kind: "syscall_enter".into(),
            thread: 100,
            address: 0x7FF6_0000_1000,
            args: vec![0x12, 0x34],
            repeats: 1,
        };
        let text = serde_json::to_string(&view).unwrap();
        assert!(
            !text.contains("repeats"),
            "重复次数为 1 时不应出现在 JSON 里: {text}"
        );
    }

    #[test]
    fn capability_names_are_unique_and_stable() {
        let names: Vec<&str> = Capability::ALL
            .iter()
            .copied()
            .map(Capability::name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "能力短名必须唯一");
        assert_eq!(names[0], "mem_read");
        assert_eq!(names[names.len() - 1], "capability_grant");
    }

    #[test]
    fn capability_set_serializes_with_names() {
        let caps = CapabilitySet::from_iter([Capability::MemRead, Capability::Breakpoint]);
        let text = serde_json::to_string(&caps).unwrap();
        assert!(text.contains("\"mem_read\""));
        assert!(text.contains("\"breakpoint\""));
        assert_eq!(serde_json::from_str::<CapabilitySet>(&text).unwrap(), caps);
    }

    #[test]
    fn response_is_self_describing() {
        let resp = Response {
            id: 7,
            body: ResponseBody::Accepted { epoch: 3 },
        };
        let text = serde_json::to_string(&resp).unwrap();
        assert!(text.contains("\"kind\":\"accepted\""));
        assert!(text.contains("\"id\":7"));
    }
}
