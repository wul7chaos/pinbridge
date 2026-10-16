//! 会话与目标描述。
//!
//! 一次"会话"= 一个目标进程 + 一组脚本 + 一份轨迹 + 一份权限与审计记录。
//! 我们坚持"一个会话一个目标"，不支持在一个会话里挂多个进程：
//! 一旦允许多目标，控制权、断点地址空间、轨迹序号的语义都会变得模糊，
//! 而模糊的语义正是 AI 出错时最难查的那类问题。

use crate::capability::{Capability, CapabilitySet, ControlLease};
use crate::error::{Error, Result};
use crate::id::{AgentId, ProcessId, ScriptId, SessionId};
use crate::principal::Principal;
use serde::{Deserialize, Serialize};

/// 目标程序的描述。`image_sha256` 是可选的但强烈建议提供：
/// 没有它，两次分析之间的"是不是同一份样本"就只能靠猜。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSpec {
    /// 可执行文件绝对路径（附加模式下由 agent 反查得到）。
    pub image_path: String,
    /// 目标架构：`x86_64` / `i386` / `aarch64`。
    pub arch: String,
    /// 样本哈希（hex，小写）。未知时为空串。
    pub image_sha256: String,
    /// 命令行（用于复现启动条件）。
    pub command_line: String,
    /// 是否由 PinBridge 启动（`true`）还是附加到已运行进程（`false`）。
    pub launched: bool,
}

impl TargetSpec {
    /// 由路径启动的新目标。
    #[must_use]
    pub fn launch(image_path: impl Into<String>, arch: impl Into<String>) -> Self {
        Self {
            image_path: image_path.into(),
            arch: arch.into(),
            image_sha256: String::new(),
            command_line: String::new(),
            launched: true,
        }
    }

    /// 附加到已在运行的进程。
    #[must_use]
    pub fn attach(image_path: impl Into<String>, arch: impl Into<String>) -> Self {
        Self {
            launched: false,
            ..Self::launch(image_path, arch)
        }
    }

    /// 补齐哈希与命令行（链式）。
    #[must_use]
    pub fn with_details(
        mut self,
        image_sha256: impl Into<String>,
        command_line: impl Into<String>,
    ) -> Self {
        self.image_sha256 = image_sha256.into();
        self.command_line = command_line.into();
        self
    }
}

/// 会话状态机。
///
/// ```text
/// Created ──attach──▶ Attached ──run──▶ Running ⇄ Paused
///    │                   │                 │        │
///    └──────fail────────▶ Failed            └─stop───┴──▶ Detached ──▶ Closed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// 已创建，尚未接触目标（此时还不允许加载脚本）。
    Created,
    /// agent 已注入，目标暂停在入口。
    Attached,
    /// 目标在跑。
    Running,
    /// 目标被中断，等待分析者决策。
    Paused,
    /// 连接断开但会话记录保留，可尝试重新附加。
    Detached,
    /// 会话正常结束。
    Closed,
    /// 不可恢复失败。
    Failed,
}

impl SessionState {
    /// 是否允许下发加载脚本的请求。
    #[must_use]
    pub const fn accepts_script_load(self) -> bool {
        matches!(self, Self::Attached | Self::Paused)
    }

    /// 是否可以接受控制命令。
    #[must_use]
    pub const fn accepts_control(self) -> bool {
        matches!(self, Self::Attached | Self::Running | Self::Paused)
    }

    /// 是否已终结。
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }
}

/// 一次分析会话。
#[derive(Debug, Clone)]
pub struct Session {
    /// 会话 ID。
    pub id: SessionId,
    /// 目标描述。
    pub target: TargetSpec,
    /// 目标 PID（附加前为 0）。
    pub pid: ProcessId,
    /// 状态。
    pub state: SessionState,
    /// 创建时间（Unix 毫秒）。
    pub created_at_ms: u64,
    /// 已加载脚本的 ID 列表（顺序即加载顺序）。
    pub scripts: Vec<ScriptId>,
    /// 参与本次会话的 AI 客户端。
    pub agents: Vec<AgentId>,
    /// 控制权租约。
    pub lease: ControlLease,
    /// 会话级能力上限。任何主体拿到的能力都必须是它的子集。
    pub ceiling: CapabilitySet,
    /// 轨迹落盘路径（未开始录制时为空）。
    pub trace_path: String,
}

impl Session {
    /// 创建会话。调用方（HubService）负责分配 ID 并做权限初始化。
    #[must_use]
    pub fn new(id: SessionId, target: TargetSpec, created_at_ms: u64) -> Self {
        Self {
            id,
            target,
            pid: ProcessId::new(0),
            state: SessionState::Created,
            created_at_ms,
            scripts: Vec::new(),
            agents: Vec::new(),
            // 初始租约给人类操作员，epoch 从 1 开始（0 保留给"未初始化"）
            lease: ControlLease::grant(
                Principal::human("operator"),
                1,
                std::time::Duration::from_secs(120),
            ),
            ceiling: Capability::operator_default(),
            trace_path: String::new(),
        }
    }

    /// 状态迁移。非法迁移返回错误而不是静默忽略——静默忽略会让 AI 以为
    /// 自己的命令生效了。
    pub fn transition(&mut self, next: SessionState) -> Result<()> {
        use SessionState::{Attached, Closed, Created, Detached, Failed, Paused, Running};
        let ok = match (self.state, next) {
            (Created, Attached | Failed | Closed) => true,
            (Attached, Running | Paused | Detached | Failed | Closed) => true,
            (Running, Paused | Detached | Failed | Closed) => true,
            (Paused, Running | Detached | Failed | Closed) => true,
            (Detached, Attached | Closed | Failed) => true,
            (Failed | Closed, _) => false,
            (same, next) if same == next => true,
            _ => false,
        };
        if ok {
            self.state = next;
            Ok(())
        } else {
            Err(Error::TargetUnavailable(format!(
                "非法状态迁移 {:?} -> {:?}（会话 {}）",
                self.state, next, self.id
            )))
        }
    }

    /// 记录一次脚本加载。
    pub fn note_script_loaded(&mut self, script: ScriptId) {
        self.scripts.push(script);
    }

    /// 记录一个 agent 加入。
    pub fn note_agent(&mut self, agent: AgentId) {
        if !self.agents.contains(&agent) {
            self.agents.push(agent);
        }
    }

    /// 生效能力 = 主体请求 ∩ 会话上限。
    #[must_use]
    pub fn effective_capabilities(&self, requested: CapabilitySet) -> CapabilitySet {
        self.ceiling.intersect(requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Session {
        Session::new(
            SessionId::new(1),
            TargetSpec::launch("TARGET.exe", "x86_64"),
            1_700_000_000_000,
        )
    }

    #[test]
    fn state_machine_rejects_illegal_transitions() {
        let mut s = fixture();
        assert!(s.transition(SessionState::Attached).is_ok());
        assert!(s.transition(SessionState::Running).is_ok());
        assert!(
            s.transition(SessionState::Created).is_err(),
            "Running 不能回到 Created"
        );
        assert!(s.transition(SessionState::Paused).is_ok());
        assert!(s.transition(SessionState::Closed).is_ok());
        assert!(
            s.transition(SessionState::Running).is_err(),
            "Closed 是终结态"
        );
        assert!(s.state.is_terminal());
    }

    #[test]
    fn script_load_only_before_running() {
        let mut s = fixture();
        assert!(!s.state.accepts_script_load());
        s.transition(SessionState::Attached).unwrap();
        assert!(s.state.accepts_script_load());
        s.transition(SessionState::Running).unwrap();
        assert!(
            !s.state.accepts_script_load(),
            "运行中不接受新脚本，必须先暂停"
        );
    }

    #[test]
    fn capabilities_are_capped_by_session_ceiling() {
        let mut s = fixture();
        s.ceiling = Capability::read_only_baseline();
        let asked = Capability::operator_default();
        let got = s.effective_capabilities(asked);
        assert!(got.is_subset_of(s.ceiling));
        assert!(!got.contains(crate::Capability::MemWrite));
    }

    #[test]
    fn agents_are_deduplicated() {
        let mut s = fixture();
        s.note_agent(AgentId::new(3));
        s.note_agent(AgentId::new(3));
        assert_eq!(s.agents.len(), 1);
    }
}
