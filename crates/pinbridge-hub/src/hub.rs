//! HubService：控制面实现。
//!
//! ## 一个决定：所有写操作都先过"三关"
//!
//! 任何会改变目标状态的请求，都必须依次通过：
//!
//! 1. **能力**：主体是否被授予了这类动作（`CapabilitySet::require`）；
//! 2. **控制权**：请求携带的 epoch 是否等于当前租约 epoch；
//! 3. **状态**：会话状态机是否允许该动作（例如 `Running` 时不允许加载脚本）。
//!
//! 三关都过不了就返回结构化错误，并把**失败本身**写进溯源日志。
//! 这个顺序不是随便定的：能力判断最便宜且与目标无关（可以完全本地拒绝），
//! 控制权次之，状态判断最贵（要查会话）。把拒绝做在链路最前面，
//! 可以让"AI 疯狂重试无权限操作"不会对目标进程产生任何副作用。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_core::error::{Error, Result};
use pinbridge_core::id::{EventSeq, ProcessId, ScriptId, SessionId};
use pinbridge_core::principal::{Principal, PrincipalKind};
use pinbridge_core::session::{Session, SessionState, TargetSpec};
use pinbridge_core::time::{format_unix_millis_utc, now_unix_millis};
use pinbridge_proto::frames::{ControlCommand, EventView, SessionSummary};
use pinbridge_trace::event::{Event, EventKind};
use pinbridge_trace::pbt::{Summary, FLAG_REPEAT_FOLDED};

use crate::provenance::ProvenanceLog;
use crate::runtime::SessionRuntime;
use crate::script::{sha256_hex, Script, ScriptRegistry};

/// 一个会话的全部状态。
#[derive(Debug)]
struct SessionEntry {
    session: Session,
    runtime: SessionRuntime,
    scripts: ScriptRegistry,
    /// 到目标端的**主动请求**通道（读内存等）。
    ///
    /// 为什么是可选的：会话可以先被创建、稍后才挂上链路；而没有链路时
    /// 主动请求必须给出**说清缺什么**的错误，不能让调用方以为"目标上读不到"。
    link: Option<Box<dyn crate::link::TargetLink>>,
}

/// 控制面服务。单实例、非线程安全：并发由上层（MCP 服务 / Tauri 命令层）
/// 用一把锁串行化。刻意不用 `Arc<Mutex<...>>` 内部加锁——
/// "谁在什么时候持有这把锁"是调试期最容易出错的部分，把它放在边界上更好推理。
#[derive(Debug)]
pub struct HubService {
    next_session: u64,
    grants: BTreeMap<String, CapabilitySet>,
    sessions: BTreeMap<u64, SessionEntry>,
    provenance: ProvenanceLog,
    trace_dir: PathBuf,
}

impl Default for HubService {
    fn default() -> Self {
        Self::new()
    }
}

impl HubService {
    /// 默认轨迹目录：系统临时目录下的 `pinbridge-traces`。
    #[must_use]
    pub fn new() -> Self {
        Self::with_trace_dir(std::env::temp_dir().join("pinbridge-traces"))
    }

    /// 指定轨迹目录。
    #[must_use]
    pub fn with_trace_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            next_session: 0,
            grants: BTreeMap::new(),
            sessions: BTreeMap::new(),
            provenance: ProvenanceLog::new(),
            trace_dir: dir.as_ref().to_path_buf(),
        }
    }

    // ---------------------------------------------------------------- 权限

    /// 查询（并在首次接触时初始化）某个主体的能力集。
    ///
    /// 默认值刻意保守：人类拿到操作员全集，AI 只拿到只读基线。
    /// "AI 需要更强能力"必须由人类在一次显式授权中给出——
    /// 这条规则的意义不是不信任 AI，而是让**能力扩张一定留痕**。
    pub fn capabilities_of(&mut self, principal: &Principal) -> CapabilitySet {
        self.grants
            .entry(principal.audit_id())
            .or_insert_with(|| match principal.kind() {
                PrincipalKind::Human => Capability::operator_default(),
                PrincipalKind::Ai => Capability::read_only_baseline(),
                PrincipalKind::System => Capability::operator_default(),
            })
            .to_owned()
    }

    fn ensure(&mut self, principal: &Principal, capability: Capability) -> Result<()> {
        self.capabilities_of(principal).require(capability)
    }

    /// 授权：把 `target` 的能力收窄到 `requested` 与现有能力的交集。
    ///
    /// 只有人类能调用（AI 不能给自己或别的 agent 扩权）。
    pub fn grant_capabilities(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        target: &Principal,
        requested: &[Capability],
    ) -> Result<CapabilitySet> {
        let action = "grant_capabilities";
        let detail = format!("target={target} requested={requested:?} epoch={epoch}");
        let outcome = self.grant_capabilities_inner(actor, session, epoch, target, requested);
        match &outcome {
            Ok(narrowed) => {
                self.provenance.record_ok_in(
                    session.get(),
                    actor,
                    action,
                    format!("{detail} → {}", describe_caps(*narrowed)),
                );
            }
            Err(e) => {
                self.provenance.record_failure_in(
                    session.get(),
                    actor,
                    action,
                    detail,
                    e.to_string(),
                );
            }
        }
        outcome
    }

    fn grant_capabilities_inner(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        target: &Principal,
        requested: &[Capability],
    ) -> Result<CapabilitySet> {
        self.ensure(actor, Capability::CapabilityGrant)?;
        if actor.kind() != PrincipalKind::Human {
            return Err(Error::CapabilityDenied {
                needed: Capability::CapabilityGrant,
                held: self.capabilities_of(actor),
            });
        }
        self.check_epoch(session, epoch, actor)?;
        let ceiling = self
            .sessions
            .get(&session.get())
            .map_or(CapabilitySet::empty(), |e| e.session.ceiling);
        // 授权语义：人类做**绝对设定**，但永远不能超过会话上限（ceiling）。
        // 没有 ceiling 这道闸，"给 AI 开一次权限"就会变成"给 AI 开永远的权力"。
        let narrowed = CapabilitySet::from_iter(requested.iter().copied()).intersect(ceiling);
        self.grants.insert(target.audit_id(), narrowed);
        Ok(narrowed)
    }

    /// 调整会话的能力上限（收窄时同步作用于所有已授权主体）。仅人类可调用。
    ///
    /// 这是"人类保留最终阀门"的具体落点：即使某个 agent 已经拿到了 `MemWrite`，
    /// 人类把 ceiling 收窄之后它的下一次写入会立刻被拒——
    /// 不需要去逐个追回已经发出去的授权。
    pub fn set_ceiling(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        caps: &[Capability],
    ) -> Result<CapabilitySet> {
        if actor.kind() != PrincipalKind::Human {
            return Err(Error::CapabilityDenied {
                needed: Capability::CapabilityGrant,
                held: self.capabilities_of(actor),
            });
        }
        self.check_epoch(session, epoch, actor)?;
        let new_ceiling = CapabilitySet::from_iter(caps.iter().copied());
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        entry.session.ceiling = new_ceiling;
        // 收窄所有**被授权方**的能力。刻意跳过发起这次变更的操作员本人：
        // ceiling 约束的是"能委托出去什么"，如果连权威自己的 CapabilityGrant
        // 都被 ceiling 收掉，就再也无法把 ceiling 调回来——能力会单向棘轮到
        // 一个谁都无法恢复的状态，等于把工具锁死。
        let authority = actor.audit_id();
        for (key, granted) in self.grants.iter_mut() {
            if *key == authority {
                continue;
            }
            *granted = granted.intersect(new_ceiling);
        }
        self.provenance.record_ok_in(
            session.get(),
            actor,
            "set_ceiling",
            describe_caps(new_ceiling),
        );
        Ok(new_ceiling)
    }

    fn check_epoch(&self, session: SessionId, epoch: u64, actor: &Principal) -> Result<()> {
        let Some(entry) = self.sessions.get(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        if entry.session.lease.accepts(epoch) {
            Ok(())
        } else {
            Err(Error::ControlConflict {
                holder: format!("{} (请求方 {actor})", entry.session.lease.holder()),
                current_epoch: entry.session.lease.epoch(),
                requested_epoch: epoch,
            })
        }
    }

    // ---------------------------------------------------------------- 会话

    /// 新建会话（启动目标）。
    pub fn create_session(&mut self, actor: &Principal, target: TargetSpec) -> Result<SessionId> {
        self.ensure(actor, Capability::ProcessControl)?;
        self.next_session += 1;
        let id = SessionId::new(self.next_session);
        let mut session = Session::new(id, target.clone(), now_unix_millis());
        // 控制权交给"创建这个会话的人/agent"，而不是某个匿名的默认操作员：
        // 审计里必须能看到具体是谁。
        session.lease = pinbridge_core::capability::ControlLease::grant(
            actor.clone(),
            1,
            std::time::Duration::from_secs(900),
        )
        .with_capabilities(Capability::operator_default());
        let entry = SessionEntry {
            session,
            runtime: SessionRuntime::new(),
            scripts: ScriptRegistry::new(),
            // 会话先建、链路后挂：创建时还没有"能通话"的东西，
            // 挂了链路才能发主动请求（见 attach_link）。
            link: None,
        };
        self.sessions.insert(id.get(), entry);
        self.provenance.record_ok(
            actor,
            "create_session",
            format!(
                "session={id} image={} arch={}",
                target.image_path, target.arch
            ),
        );
        Ok(id)
    }

    /// 附加到已运行进程。
    pub fn attach(
        &mut self,
        actor: &Principal,
        session: SessionId,
        pid: u32,
        command_line: &str,
    ) -> Result<()> {
        self.ensure(actor, Capability::ProcessControl)?;
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        entry.session.pid = ProcessId::new(pid);
        entry.session.target.command_line = command_line.to_owned();
        entry.session.transition(SessionState::Attached)?;
        self.provenance.record_ok_in(
            session.get(),
            actor,
            "attach",
            format!("pid={pid} cmdline={command_line}"),
        );
        Ok(())
    }

    /// 关闭会话（释放录制器）。
    pub fn close_session(&mut self, actor: &Principal, session: SessionId) -> Result<()> {
        // 先把录制器收尾（借用范围刻意收窄，避免与溯源日志的写入互相借自 self）
        let summary = {
            let entry = self
                .sessions
                .get_mut(&session.get())
                .ok_or_else(|| Error::TargetUnavailable(format!("会话 {session} 不存在")))?;
            if entry.runtime.is_recording() {
                Some(entry.runtime.stop_trace()?)
            } else {
                None
            }
        };
        if let Some(summary) = summary {
            self.provenance.record_ok_in(
                session.get(),
                actor,
                "stop_trace",
                format!(
                    "(隐式收尾) records={} events={}",
                    summary.records, summary.events
                ),
            );
        }
        let entry = self
            .sessions
            .get_mut(&session.get())
            .ok_or_else(|| Error::TargetUnavailable(format!("会话 {session} 不存在")))?;
        entry.session.transition(SessionState::Closed)?;
        self.provenance
            .record_ok_in(session.get(), actor, "close_session", "ok");
        Ok(())
    }

    /// 会话摘要列表（给 CLI / 工作台 / MCP 的 `pb_session_list`）。
    #[must_use]
    pub fn sessions(&self) -> Vec<SessionSummary> {
        self.sessions
            .values()
            .map(|entry| SessionSummary {
                session: entry.session.id.get(),
                pid: entry.session.pid,
                state: entry.session.state,
                image_path: entry.session.target.image_path.clone(),
                lease_holder: entry.session.lease.holder().to_string(),
                scripts: entry.scripts.live().count(),
            })
            .collect()
    }

    /// 取某会话的引用。
    #[must_use]
    pub fn session(&self, session: SessionId) -> Option<&Session> {
        self.sessions.get(&session.get()).map(|e| &e.session)
    }

    /// 取某会话的脚本列表。
    #[must_use]
    pub fn scripts(&self, session: SessionId) -> Option<&[Script]> {
        self.sessions.get(&session.get()).map(|e| e.scripts.all())
    }

    /// 当前控制权 epoch。
    pub fn epoch_of(&self, session: SessionId) -> Result<u64> {
        self.sessions
            .get(&session.get())
            .map(|e| e.session.lease.epoch())
            .ok_or_else(|| Error::TargetUnavailable(format!("会话 {session} 不存在")))
    }

    /// 人类硬接管：收回当前租约、推进 epoch。返回新 epoch。
    ///
    /// 这是"人与 AI 平级"的具体体现：人类不需要 AI 配合就能拿回控制权，
    /// 而 AI 的在途命令会立刻失效（下次提交时收到 `ControlConflict`）。
    pub fn takeover(&mut self, actor: &Principal, session: SessionId) -> Result<u64> {
        if actor.kind() != PrincipalKind::Human {
            return Err(Error::CapabilityDenied {
                needed: Capability::CapabilityGrant,
                held: self.capabilities_of(actor),
            });
        }
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        let previous = entry.session.lease.epoch();
        entry.session.lease.revoke();
        let next = previous + 1;
        entry.session.lease.renew(actor.clone(), next);
        self.provenance.record_ok_in(
            session.get(),
            actor,
            "takeover",
            format!("epoch {previous} → {next}"),
        );
        Ok(next)
    }

    // ---------------------------------------------------------------- 控制

    /// 下发控制命令。
    pub fn control(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        command: &ControlCommand,
    ) -> Result<()> {
        let action = format!("control_{}", command_tag(command));
        let detail = format!("{command:?} epoch={epoch}");
        let outcome = self.control_inner(actor, session, epoch, command);
        match &outcome {
            Ok(()) => self
                .provenance
                .record_ok_in(session.get(), actor, action, detail),
            Err(e) => self.provenance.record_failure_in(
                session.get(),
                actor,
                action,
                detail,
                e.to_string(),
            ),
        };
        outcome
    }

    fn control_inner(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        command: &ControlCommand,
    ) -> Result<()> {
        self.ensure(actor, required_capability(command))?;
        self.check_epoch(session, epoch, actor)?;
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        if !entry.session.state.accepts_control() {
            return Err(Error::TargetUnavailable(format!(
                "会话 {session} 当前状态 {:?} 不接受控制命令",
                entry.session.state
            )));
        }
        match command {
            ControlCommand::Continue => {
                entry.session.transition(SessionState::Running)?;
                entry.runtime.record(
                    EventKind::StateChange,
                    0,
                    0,
                    &[(0, state_code("running"))],
                )?;
            }
            ControlCommand::Pause => {
                entry.session.transition(SessionState::Paused)?;
                entry
                    .runtime
                    .record(EventKind::StateChange, 0, 0, &[(0, state_code("paused"))])?;
            }
            ControlCommand::Step { count } => {
                entry.session.transition(SessionState::Paused)?;
                entry.runtime.record(
                    EventKind::StateChange,
                    0,
                    0,
                    &[(0, state_code("step")), (1, u64::from(*count))],
                )?;
            }
            ControlCommand::RunTo { address } => {
                entry.session.transition(SessionState::Paused)?;
                entry.runtime.record(
                    EventKind::StateChange,
                    0,
                    *address,
                    &[(0, state_code("run_to"))],
                )?;
            }
            ControlCommand::BreakpointSet { address, .. } => {
                entry.session.transition(SessionState::Paused)?;
                entry.runtime.record(
                    EventKind::StateChange,
                    0,
                    *address,
                    &[(0, state_code("breakpoint_set"))],
                )?;
            }
            ControlCommand::BreakpointClear { address } => {
                entry.runtime.record(
                    EventKind::StateChange,
                    0,
                    *address,
                    &[(0, state_code("breakpoint_clear"))],
                )?;
            }
            ControlCommand::PatternHook { .. } => {
                entry.session.transition(SessionState::Paused)?;
                entry.runtime.record(
                    EventKind::PatternHit,
                    0,
                    0,
                    &[(0, state_code("pattern_hook"))],
                )?;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- 脚本

    /// 加载脚本。返回脚本 ID。
    pub fn load_script(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        name: &str,
        source: &str,
        subscribe: Vec<EventKind>,
    ) -> Result<ScriptId> {
        let hash = sha256_hex(source.as_bytes());
        let action = "load_script";
        let detail = format!("name={name} sha256={hash} bytes={}", source.len());
        let outcome = self.load_script_inner(actor, session, epoch, name, source, subscribe);
        match &outcome {
            Ok(id) => {
                let _ = self.provenance.record_ok_in(
                    session.get(),
                    actor,
                    action,
                    format!("{detail} script={id}"),
                );
            }
            Err(e) => {
                let _ = self.provenance.record_failure_in(
                    session.get(),
                    actor,
                    action,
                    detail,
                    e.to_string(),
                );
            }
        }
        outcome
    }

    fn load_script_inner(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        name: &str,
        source: &str,
        subscribe: Vec<EventKind>,
    ) -> Result<ScriptId> {
        self.ensure(actor, Capability::ScriptLoad)?;
        self.check_epoch(session, epoch, actor)?;
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        if !entry.session.state.accepts_script_load() {
            return Err(Error::Script(format!(
                "会话 {session} 状态 {:?} 不接受新脚本（请先 pause）",
                entry.session.state
            )));
        }
        let script = entry.scripts.load(name, source, subscribe)?;
        entry.session.note_script_loaded(script.id);
        Ok(script.id)
    }

    /// 卸载脚本。
    pub fn unload_script(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        id: ScriptId,
    ) -> Result<()> {
        let action = "unload_script";
        let detail = format!("script={id} epoch={epoch}");
        let outcome: Result<()> = (|| {
            self.ensure(actor, Capability::ScriptLoad)?;
            self.check_epoch(session, epoch, actor)?;
            let entry = self
                .sessions
                .get_mut(&session.get())
                .ok_or_else(|| Error::TargetUnavailable(format!("会话 {session} 不存在")))?;
            entry.scripts.unload(id)?;
            Ok(())
        })();
        match &outcome {
            Ok(()) => self
                .provenance
                .record_ok_in(session.get(), actor, action, detail),
            Err(e) => self.provenance.record_failure_in(
                session.get(),
                actor,
                action,
                detail,
                e.to_string(),
            ),
        };
        outcome
    }

    // ---------------------------------------------------------------- 事件

    /// agent 侧推入一条事件（M1 起由 IPC 调用，M0 由测试与 CLI 直接调用）。
    pub fn push_event(
        &mut self,
        session: SessionId,
        kind: EventKind,
        thread_id: u32,
        address: u64,
        args: &[(usize, u64)],
    ) -> Result<EventSeq> {
        let entry = self
            .sessions
            .get_mut(&session.get())
            .ok_or_else(|| Error::TargetUnavailable(format!("会话 {session} 不存在")))?;
        entry.runtime.record(kind, thread_id, address, args)
    }

    /// 按序号查询事件（AI 的主要观察手段）。
    ///
    /// 返回值第三项是"被丢弃的事件数"，必须原样转达给调用方：
    /// 一份悄悄少了 3 万条事件的答案比一个错误更危险。
    pub fn query_events(
        &mut self,
        actor: &Principal,
        session: SessionId,
        from: u64,
        limit: usize,
        fold: bool,
    ) -> Result<(Vec<EventView>, u64, u64)> {
        self.ensure(actor, Capability::MemRead)?;
        let Some(entry) = self.sessions.get(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        let (events, next, _lossy) = entry.runtime.query(from, limit, fold);
        let dropped = entry.runtime.channel_dropped() + entry.runtime.window_dropped();
        let views = events.iter().map(to_view).collect();
        Ok((views, next, dropped))
    }

    /// 当前积压（脚本消费不过来时持续增长）。
    pub fn backlog(&self, session: SessionId) -> Option<usize> {
        self.sessions
            .get(&session.get())
            .map(|e| e.runtime.backlog())
    }

    // -------------------------------------------------- 主动请求（到目标端）

    /// 给会话挂上一条到目标端的链路。
    ///
    /// 需要 `ProcessControl`：挂链路会**扩大这个会话能做的事**（从"只看事件"
    /// 变成"能主动读目标内存"），所以它和创建会话是同一档操作。
    ///
    /// # Errors
    ///
    /// 能力不足、会话不存在，或会话已终结。
    pub fn attach_link(
        &mut self,
        actor: &Principal,
        session: SessionId,
        link: Box<dyn crate::link::TargetLink>,
    ) -> Result<String> {
        self.ensure(actor, Capability::ProcessControl)?;
        let described = link.describe();
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        if entry.session.state.is_terminal() {
            return Err(Error::TargetUnavailable(format!(
                "会话 {session} 状态 {:?} 已终结，不再接受链路挂载",
                entry.session.state
            )));
        }
        entry.link = Some(link);
        Ok(described)
    }

    /// 会话当前是否挂了到目标端的链路。
    #[must_use]
    pub fn has_link(&self, session: SessionId) -> bool {
        self.sessions
            .get(&session.get())
            .is_some_and(|entry| entry.link.is_some())
    }

    /// 读目标内存（通过会话的链路）。
    ///
    /// 顺序是刻意的：**先校验参数、再校验能力、最后才碰链路**。
    /// 一个 `length=0` 的请求在任何状态下都是调用方的 bug，不该被翻译成
    /// "目标不可用"——那会让人去查目标。
    ///
    /// # Errors
    ///
    /// - `Protocol`：`length == 0`；
    /// - `CapabilityDenied`：调用方没有 `mem_read`；
    /// - `TargetUnavailable`：会话不存在、已终结、或**没有挂链路**（错误文本会点明）；
    /// - 其余来自链路的错误原样透传。
    pub fn read_memory(
        &mut self,
        actor: &Principal,
        session: SessionId,
        address: u64,
        length: u32,
    ) -> Result<Vec<u8>> {
        if length == 0 {
            return Err(Error::Protocol(
                "read_memory: length 必须大于 0（传 0 不是“查一下能否读”，它只是调用方的 bug）"
                    .into(),
            ));
        }
        self.ensure(actor, Capability::MemRead)?;
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        if !entry.session.state.accepts_control() {
            return Err(Error::TargetUnavailable(format!(
                "会话 {session} 状态 {:?} 不接受读内存请求",
                entry.session.state
            )));
        }
        let Some(link) = entry.link.as_mut() else {
            return Err(Error::TargetUnavailable(format!(
                "会话 {session} 没有挂载到目标端的链路（Hub 的会话默认只有事件窗口与轨迹，\
                 主动请求需要先 attach_link）"
            )));
        };
        let bytes = link.read_memory(address, length)?;
        if bytes.len() != length as usize {
            return Err(Error::Protocol(format!(
                "read_memory: 请求 {length} 字节，链路返回 {} 字节——长度不符必须报错，\
                 否则上层会把半截数据当完整数据用",
                bytes.len()
            )));
        }
        Ok(bytes)
    }
    // ---------------------------------------------------------------- 轨迹

    /// 开始录制，返回 `.pbt` 路径。
    pub fn start_trace(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
        profile: &str,
    ) -> Result<String> {
        self.ensure(actor, Capability::TraceRecord)?;
        self.check_epoch(session, epoch, actor)?;
        if let Some(existing) = self.session(session) {
            if !existing.state.accepts_control() {
                return Err(Error::TargetUnavailable(format!(
                    "会话 {session} 状态 {:?} 不接受录制请求",
                    existing.state
                )));
            }
        }
        let folding = !matches!(profile, "none" | "");
        let flags = if folding { FLAG_REPEAT_FOLDED } else { 0 };
        let at = now_unix_millis();
        let path = self
            .trace_dir
            .join(format!("session-{}-{at}.pbt", session.get()));
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        let meta = format!(
            "{{\"tool\":\"pinbridge-hub\",\"session\":{},\"image\":{},\"arch\":{},\"image_sha256\":{},\
              \"profile\":{},\"flags\":{},\"started_at\":{},\"started_at_utc\":{}}}",
            session.get(),
            json_string(&entry.session.target.image_path),
            json_string(&entry.session.target.arch),
            json_string(&entry.session.target.image_sha256),
            json_string(profile),
            flags,
            at,
            json_string(&format_unix_millis_utc(at))
        );
        entry.runtime.start_trace(&path, &meta, folding)?;
        self.provenance.record_ok_in(
            session.get(),
            actor,
            "start_trace",
            format!("path={} profile={profile}", path.display()),
        );
        Ok(path.to_string_lossy().into_owned())
    }

    /// 停止录制，返回汇总。
    pub fn stop_trace(
        &mut self,
        actor: &Principal,
        session: SessionId,
        epoch: u64,
    ) -> Result<Summary> {
        self.ensure(actor, Capability::TraceRecord)?;
        self.check_epoch(session, epoch, actor)?;
        let Some(entry) = self.sessions.get_mut(&session.get()) else {
            return Err(Error::TargetUnavailable(format!("会话 {session} 不存在")));
        };
        let summary = entry.runtime.stop_trace()?;
        self.provenance.record_ok_in(
            session.get(),
            actor,
            "stop_trace",
            format!(
                "records={} events={} folded={} dropped={} truncated={}",
                summary.records,
                summary.events,
                summary.repeat_folded,
                summary.dropped,
                summary.truncated
            ),
        );
        Ok(summary)
    }

    // ---------------------------------------------------------------- 溯源

    /// 某会话的操作溯源（JSON Lines 形态，可直接塞进 JSON 响应）。
    #[must_use]
    pub fn provenance_of(&self, session: SessionId) -> Vec<String> {
        self.provenance
            .for_session(session.get())
            .into_iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect()
    }

    /// 全量溯源日志（CLI / 工作台初始视图）。
    #[must_use]
    pub fn provenance_lines(&self) -> Vec<String> {
        self.provenance.to_json_lines()
    }

    /// 轨迹目录。
    #[must_use]
    pub fn trace_dir(&self) -> &Path {
        &self.trace_dir
    }
}

/// 命令短名（进入审计的 `action` 字段）。
fn command_tag(command: &ControlCommand) -> &'static str {
    match command {
        ControlCommand::Continue => "continue",
        ControlCommand::Pause => "pause",
        ControlCommand::Step { .. } => "step",
        ControlCommand::RunTo { .. } => "run_to",
        ControlCommand::BreakpointSet { .. } => "breakpoint_set",
        ControlCommand::BreakpointClear { .. } => "breakpoint_clear",
        ControlCommand::PatternHook { .. } => "pattern_hook",
    }
}

/// 命令所需能力。
fn required_capability(command: &ControlCommand) -> Capability {
    match command {
        ControlCommand::Continue | ControlCommand::Pause => Capability::ProcessControl,
        ControlCommand::Step { .. } | ControlCommand::RunTo { .. } => Capability::SingleStep,
        ControlCommand::BreakpointSet { .. } | ControlCommand::BreakpointClear { .. } => {
            Capability::Breakpoint
        }
        ControlCommand::PatternHook { .. } => Capability::PatternHook,
    }
}

/// 状态机事件的 `args[0]`：用短名而不是判别值，方便人读轨迹。
fn state_code(state: &str) -> u64 {
    state.bytes().fold(0u64, |acc, b| (acc << 8) | u64::from(b)) & 0xFFFF_FFFF_FFFF
}

/// 能力集的可读摘要。
fn describe_caps(caps: CapabilitySet) -> String {
    if caps.is_empty() {
        return "∅（全部收回）".to_owned();
    }
    caps.iter()
        .map(Capability::name)
        .collect::<Vec<_>>()
        .join("|")
}

/// 极简 JSON 字符串转义（避免为一个字段引入额外依赖）。
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 事件 → 协议视图。
fn to_view(event: &Event) -> EventView {
    let used = event
        .args
        .iter()
        .rposition(|v| *v != 0)
        .map_or(0, |i| i + 1);
    let repeats = if event.kind == EventKind::Repeat {
        event.arg(0) as u32
    } else {
        1
    };
    EventView {
        seq: EventSeq::new(event.sequence),
        kind: event.kind.name().to_owned(),
        thread: event.thread_id,
        address: event.address,
        args: event.args[..used].to_vec(),
        repeats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO_SOURCE: &str = "# pinbridge demo hook\nprint('hello')\n";

    fn temp_hub(name: &str) -> HubService {
        let dir = std::env::temp_dir().join(format!("pb-hub-{}-{name}", std::process::id()));
        HubService::with_trace_dir(dir)
    }

    /// 一条只存在于测试里的链路：按预设值回答读内存。
    ///
    /// 这正是 `TargetLink` 存在的理由——Hub 的闸门逻辑（能力、会话状态、
    /// 错误翻译）必须能在没有目标、没有 socket 的情况下被测到。
    #[derive(Debug)]
    struct FakeLink {
        bytes: Vec<u8>,
        calls: usize,
    }

    impl crate::link::TargetLink for FakeLink {
        fn read_memory(&mut self, _address: u64, length: u32) -> Result<Vec<u8>> {
            self.calls += 1;
            let mut out = self.bytes.clone();
            out.resize(length as usize, 0);
            Ok(out)
        }

        fn describe(&self) -> String {
            "测试用假链路".into()
        }
    }

    /// 一条"少回一个字节"的链路：用来测长度校验。
    #[derive(Debug)]
    struct ShortLink;

    impl crate::link::TargetLink for ShortLink {
        fn read_memory(&mut self, _address: u64, _length: u32) -> Result<Vec<u8>> {
            Ok(vec![0x90])
        }

        fn describe(&self) -> String {
            "测试用短读链路".into()
        }
    }
    fn attached_session(hub: &mut HubService, actor: &Principal) -> SessionId {
        let id = hub
            .create_session(actor, TargetSpec::launch("TARGET.exe", "x86_64"))
            .unwrap();
        hub.attach(actor, id, 4242, "TARGET.exe --demo").unwrap();
        id
    }

    #[test]
    fn ai_starts_read_only_and_cannot_escalate_itself() {
        let mut hub = temp_hub("caps");
        let human = Principal::human("alice");
        let ai = Principal::ai("agent-7");
        let id = attached_session(&mut hub, &human);

        assert_eq!(hub.capabilities_of(&ai), Capability::read_only_baseline());
        let epoch = hub.epoch_of(id).unwrap();
        // AI 拿人类的能力集去给自己"授权" → 必须被拒
        let err = hub
            .grant_capabilities(&ai, id, epoch, &ai, &[Capability::MemWrite])
            .unwrap_err();
        assert!(matches!(err, Error::CapabilityDenied { .. }));
        assert_eq!(hub.capabilities_of(&ai), Capability::read_only_baseline());

        // 人类授权后 AI 才有写能力，且不能超过会话上限
        let granted = hub
            .grant_capabilities(&human, id, epoch, &ai, &Capability::ALL)
            .unwrap();
        assert!(granted.contains(Capability::MemWrite));
        assert!(granted.contains(Capability::ScriptLoad));
        assert_eq!(granted, Capability::operator_default());
    }

    #[test]
    fn grants_are_bounded_by_the_session_ceiling() {
        let mut hub = temp_hub("grant-ceiling");
        let human = Principal::human("alice");
        let ai = Principal::ai("agent-7");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();

        // 收窄会话上限 → 后续授权不能越过它，同时把已发出的授权一并收窄
        let ceiling = hub
            .set_ceiling(
                &human,
                id,
                epoch,
                &[Capability::MemRead, Capability::Breakpoint],
            )
            .unwrap();
        assert_eq!(
            ceiling,
            CapabilitySet::from_iter([Capability::MemRead, Capability::Breakpoint])
        );

        let granted = hub
            .grant_capabilities(&human, id, epoch, &ai, &Capability::ALL)
            .unwrap();
        assert!(granted.contains(Capability::Breakpoint));
        assert!(granted.contains(Capability::MemRead));
        assert!(!granted.contains(Capability::MemWrite), "不得越过 ceiling");
        assert!(!granted.contains(Capability::ScriptLoad));

        // 人类自己也不能越过（能力集与会话上限取交集）
        let emptied = hub.grant_capabilities(&human, id, epoch, &ai, &[]).unwrap();
        assert!(emptied.is_empty());
        assert_eq!(hub.capabilities_of(&ai), CapabilitySet::empty());

        // 权威自身的能力不被 ceiling 自动回收 —— 否则 ceiling 无法再被调回来
        assert!(hub
            .capabilities_of(&human)
            .contains(Capability::CapabilityGrant));
        let restored = hub
            .set_ceiling(&human, id, epoch, &Capability::ALL)
            .unwrap();
        assert_eq!(restored, Capability::operator_default());
    }

    #[test]
    fn stale_epoch_commands_are_rejected_after_takeover() {
        let mut hub = temp_hub("epoch");
        let human = Principal::human("alice");
        let ai = Principal::ai("agent-7");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();
        hub.grant_capabilities(&human, id, epoch, &ai, &Capability::ALL)
            .unwrap();

        let taken = hub.takeover(&human, id).unwrap();
        assert_eq!(taken, epoch + 1);
        assert_eq!(
            hub.session(id).unwrap().lease.holder().kind(),
            PrincipalKind::Human
        );

        let err = hub
            .control(&ai, id, epoch, &ControlCommand::Continue)
            .unwrap_err();
        assert!(
            matches!(err, Error::ControlConflict { .. }),
            "旧 epoch 必须被拒: {err:?}"
        );
        hub.control(&human, id, taken, &ControlCommand::Continue)
            .unwrap();
        assert_eq!(hub.session(id).unwrap().state, SessionState::Running);
    }

    #[test]
    fn ai_cannot_take_over_control() {
        let mut hub = temp_hub("ai-takeover");
        let human = Principal::human("alice");
        let ai = Principal::ai("agent-7");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();
        hub.grant_capabilities(&human, id, epoch, &ai, &Capability::ALL)
            .unwrap();
        let err = hub.takeover(&ai, id).unwrap_err();
        assert!(matches!(err, Error::CapabilityDenied { .. }));
        assert_eq!(hub.epoch_of(id).unwrap(), epoch);
    }

    #[test]
    fn script_load_requires_paused_state_and_is_traced_with_hash() {
        let mut hub = temp_hub("script");
        let human = Principal::human("alice");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();

        let script = hub
            .load_script(
                &human,
                id,
                epoch,
                "hook.py",
                DEMO_SOURCE,
                vec![EventKind::BreakpointHit],
            )
            .unwrap();
        let recorded = &hub.scripts(id).unwrap()[0];
        assert_eq!(script.get(), 1);
        assert_eq!(
            recorded.source_sha256,
            "a98c7127446444ec31a9890062ee7f669d02b955bf07ea513f23f5f3db81671e"
        );

        hub.control(&human, id, epoch, &ControlCommand::Continue)
            .unwrap();
        let err = hub
            .load_script(&human, id, epoch, "hook2.py", DEMO_SOURCE, vec![])
            .unwrap_err();
        assert!(err.to_string().contains("不接受新脚本"));

        let trace = hub.provenance_of(id);
        assert!(
            trace.iter().any(|line| line.contains("load_script")),
            "加载脚本必须留痕"
        );
        assert!(trace
            .iter()
            .any(|line| line.contains("能力不足") || line.contains("不接受新脚本")));
    }

    #[test]
    fn breakpoint_control_records_state_change_events() {
        let mut hub = temp_hub("bp");
        let human = Principal::human("alice");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();
        hub.control(
            &human,
            id,
            epoch,
            &ControlCommand::BreakpointSet {
                address: 0x1400_01000,
                label: "oep".into(),
            },
        )
        .unwrap();
        hub.control(&human, id, epoch, &ControlCommand::Step { count: 4 })
            .unwrap();

        let (events, next, dropped) = hub.query_events(&human, id, 1, 16, false).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "state_change");
        assert_eq!(events[0].address, 0x1400_01000);
        assert_eq!(events[1].args[1], 4, "step 的步数应被记录");
        assert_eq!(next, 3);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn trace_pipeline_writes_and_summarises() {
        let mut hub = temp_hub("trace");
        let human = Principal::human("alice");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();
        let path = hub.start_trace(&human, id, epoch, "repeat+delta").unwrap();
        for _ in 0..500 {
            hub.push_event(
                id,
                EventKind::SyscallEnter,
                100,
                0x2000,
                &[(0, 0x40), (1, 0x3000)],
            )
            .unwrap();
        }
        let summary = hub.stop_trace(&human, id, epoch).unwrap();
        assert_eq!(summary.events, 500);
        assert_eq!(summary.repeat_folded, 499);
        assert!(summary.fold_ratio() > 0.98);

        let mut reader =
            pinbridge_trace::pbt::PbtReader::open(std::fs::File::open(&path).unwrap()).unwrap();
        let records = reader.read_all().unwrap();
        assert_eq!(
            pinbridge_trace::fold::expand_records(&records).len(),
            500,
            "轨迹必须能还原出全部 500 条事件"
        );
        assert!(reader.meta().contains("TARGET.exe"));
        let _ = std::fs::remove_file(&path);
        assert!(hub
            .provenance_of(id)
            .iter()
            .any(|l| l.contains("stop_trace")));
    }

    #[test]
    fn close_session_flushes_an_active_recorder() {
        let mut hub = temp_hub("close");
        let human = Principal::human("alice");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();
        let path = hub.start_trace(&human, id, epoch, "repeat").unwrap();
        hub.push_event(id, EventKind::BreakpointHit, 1, 0x1000, &[])
            .unwrap();
        hub.close_session(&human, id).unwrap();
        assert_eq!(hub.session(id).unwrap().state, SessionState::Closed);
        assert!(
            std::path::Path::new(&path).exists(),
            "关闭会话必须把轨迹收尾并落盘"
        );
        assert!(
            hub.start_trace(&human, id, epoch, "repeat").is_err(),
            "已关闭会话不能再录制"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn summary_and_sessions_listing_reflect_scripts() {
        let mut hub = temp_hub("listing");
        let human = Principal::human("alice");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();
        hub.load_script(&human, id, epoch, "hook.py", DEMO_SOURCE, vec![])
            .unwrap();
        let list = hub.sessions();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].scripts, 1);
        assert_eq!(list[0].pid.get(), 4242);
        assert_eq!(list[0].lease_holder, "human:alice");
        assert_eq!(hub.backlog(id), Some(0));
        assert_eq!(hub.scripts(id).unwrap().len(), 1);
        assert_eq!(hub.scripts(id).unwrap()[0].name, "hook.py");
    }

    #[test]
    fn control_requires_capability_not_just_epoch() {
        let mut hub = temp_hub("control-cap");
        let human = Principal::human("alice");
        let ai = Principal::ai("agent-7");
        let id = attached_session(&mut hub, &human);
        let epoch = hub.epoch_of(id).unwrap();
        // AI 默认只有读能力，且 epoch 正确 → 仍然必须被拒
        let err = hub
            .control(
                &ai,
                id,
                epoch,
                &ControlCommand::BreakpointSet {
                    address: 0x1000,
                    label: "x".into(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::CapabilityDenied { .. }));
        // 只读能力下查询是允许的
        assert!(hub.query_events(&ai, id, 1, 8, true).is_ok());
    }

    #[test]
    fn json_string_escapes_control_characters() {
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_string("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn state_codes_are_nonzero_and_distinct() {
        let states = [
            "running",
            "paused",
            "step",
            "run_to",
            "breakpoint_set",
            "breakpoint_clear",
            "pattern_hook",
        ];
        let codes: Vec<u64> = states.iter().map(|s| state_code(s)).collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "不同状态的编码必须不同");
        assert!(
            codes.iter().all(|c| *c != 0),
            "状态编码不得为 0（0 表示未记录）"
        );
    }
    #[test]
    fn memory_read_needs_a_capability_then_a_link() {
        let mut hub = temp_hub("read-mem-gates");
        let human = Principal::human("alice");
        let ai = Principal::ai("agent-7");
        let id = attached_session(&mut hub, &human);

        // 1) 先把 AI 的能力收窄成空集：此时报的必须是能力不足，不是"没有链路"。
        //    顺序错了会把"你没权限"说成"环境没准备好"，让人去查错的地方。
        //    （注意 AI 的**默认**集是只读基线，本身就含 mem_read，所以这里要显式收窄。）
        let epoch = hub.epoch_of(id).unwrap();
        hub.grant_capabilities(&human, id, epoch, &ai, &[]).unwrap();
        let error = hub.read_memory(&ai, id, 0x1000, 8).unwrap_err();
        assert!(
            matches!(error, Error::CapabilityDenied { .. }),
            "缺能力时应当报能力不足，实际: {error:?}"
        );

        // 2) 有能力但没挂链路：错误必须**点明缺的是链路**
        let human_error = hub.read_memory(&human, id, 0x1000, 8).unwrap_err();
        let text = human_error.to_string();
        assert!(
            text.contains("没有挂载到目标端的链路"),
            "错误必须说清缺什么，实际: {text}"
        );
        assert!(!hub.has_link(id));

        // 3) 挂上链路之后才通
        let described = hub
            .attach_link(
                &human,
                id,
                Box::new(FakeLink {
                    bytes: vec![1, 2, 3, 4, 5, 6, 7, 8],
                    calls: 0,
                }),
            )
            .unwrap();
        assert!(hub.has_link(id));
        let bytes = hub.read_memory(&human, id, 0x1000, 8).unwrap();
        assert_eq!(bytes, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        println!("已挂链路：{described}");
    }

    #[test]
    fn memory_read_validates_parameters_before_anything_else() {
        let mut hub = temp_hub("read-mem-params");
        let human = Principal::human("alice");
        let id = attached_session(&mut hub, &human);

        // length=0 是调用方的 bug：任何状态下都必须是 Protocol，
        // 而不是"目标不可用"（后者会让人去查目标）。
        let error = hub.read_memory(&human, id, 0x1000, 0).unwrap_err();
        assert!(
            matches!(error, Error::Protocol(_)),
            "length=0 应当是参数错误，实际: {error:?}"
        );

        // 会话不存在时也要能区分开
        let error = hub
            .read_memory(&human, SessionId::new(9999), 0x1000, 8)
            .unwrap_err();
        assert!(error.to_string().contains("不存在"), "实际: {error}");
    }

    #[test]
    fn a_link_that_returns_the_wrong_length_is_a_protocol_error() {
        let mut hub = temp_hub("read-mem-short");
        let human = Principal::human("alice");
        let id = attached_session(&mut hub, &human);
        hub.attach_link(&human, id, Box::new(ShortLink)).unwrap();

        let error = hub.read_memory(&human, id, 0x1000, 4).unwrap_err();
        let text = error.to_string();
        assert!(
            matches!(error, Error::Protocol(_)),
            "长度不符是协议问题，实际: {error:?}"
        );
        assert!(text.contains("长度不符"), "错误必须点明原因，实际: {text}");
    }
}
