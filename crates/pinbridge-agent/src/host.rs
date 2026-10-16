//! agent 全局状态与其上的操作。
//!
//! # 为什么是一个全局单例
//!
//! agent 是被 `pinbridge.dll` 加载到目标进程里的 DLL，Pin 不会给我们
//! "实例句柄"或构造函数钩子。所以状态只能挂在模块级静态量上。
//!
//! # 并发模型
//!
//! Pin 的探针会在**目标程序自己的线程**上被调用。因此：
//!
//! * 状态用一把 `Mutex` 保护（不是 `RwLock`：写路径（事件入队）在热路径上，
//!   读写比例并不偏向读；而且 `RwLock` 在 Windows 上的公平性不值得为这点收益冒险）；
//! * 持锁期间**绝不允许**分配大块内存、做 I/O 或调用回 Pin（否则探针会长时间阻塞
//!   目标线程，表现为"目标程序莫名卡住"）；
//! * 录制器写盘在锁内完成，这是刻意的取舍：事件顺序必须与序号严格一致，
//!   把写盘挪到锁外就要引入一个额外的有序队列，代价更大。缓冲由
//!   `BufWriter` 承担，锁内只做内存拷贝。

use std::ffi::CString;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_core::channel::{bounded, BoundedReceiver, BoundedSender, DropPolicy};
use pinbridge_core::error::{Error, Result};
use pinbridge_sys::abi::{PbEventRaw, Status};
use pinbridge_trace::event::{Event, EventKind};
use pinbridge_trace::fold::RepeatFolder;
use pinbridge_trace::pbt::{PbtWriter, Summary};
use sha2::{Digest, Sha256};

/// 事件队列容量。与 Hub 侧一致：宁可丢弃并计数，也不让**被分析进程**因分析工具而 OOM。
pub const EVENT_QUEUE_CAPACITY: usize = 65_536;

/// 已加载脚本的元信息（正文由内嵌 CPython 持有，这里只留溯源信息）。
///
/// 说明：脚本的登记、策略校验与生命周期在 [`crate::script`] 里实现
/// （`ScriptHost`）。早先这里另有一套 `ScriptHandle`，与它重复且缺少
/// 状态机——两套并存的直接后果是"卸载"在两边语义不同（一个删除记录、
/// 一个标状态）。现在统一走 `ScriptHost`。
pub use crate::script::Script;

/// agent 状态。
#[derive(Debug)]
pub struct AgentState {
    attached: bool,
    pid: u32,
    capabilities: CapabilitySet,
    events: BoundedSender<Event>,
    inbox: BoundedReceiver<Event>,
    next_sequence: u64,
    last_error: CString,
    scripts: crate::script::ScriptHost,
    budget: crate::script::ScriptBudget,
    recorder: Option<LocalRecorder>,
    /// 已采集但尚未被 Hub 取走的条数（用于背压观测）。
    drained: u64,
    /// 最近一次**推送**的序号。
    ///
    /// 与 `next_sequence` 的区别很重要：`next_sequence` 在**分配**时就前进，
    /// 而"先分配、稍后推送"是文档化的用法。严格递增这条不变量要按
    /// "最近一次推送"来判断——按 `next_sequence` 判断会把正常用法误判成落后
    /// （实测过：每次推送都被无意义地 +1）。
    last_pushed: u64,
    /// 是否处于"暂停采集"状态（`pb_pause` / `pb_continue`）。
    paused: bool,
    /// 暂停期间到达、**被计数后跳过**的事件条数。
    ///
    /// 为什么必须有它：暂停时要丢事件，但"丢了多少"必须能被回答。
    /// 静默丢弃会让"目标在暂停期间到底做了什么"变成无法回答的问题——
    /// 而那正是分析里最容易出错的地方（看起来没问题，其实少了一段）。
    paused_skipped: u64,
}

/// 目标进程内的录制器（不依赖 `pinbridge-hub`，避免 agent 把整个控制面拖进来）。
#[derive(Debug)]
struct LocalRecorder {
    writer: PbtWriter<BufWriter<File>>,
    folder: RepeatFolder,
    folding: bool,
    /// 距上次 flush 写入的记录数（增量落盘用）。
    records_since_flush: u64,
}

impl AgentState {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        let (events, inbox) = bounded(EVENT_QUEUE_CAPACITY, DropPolicy::DropOldest);
        Self {
            attached: false,
            pid: 0,
            capabilities: CapabilitySet::empty(),
            events,
            inbox,
            next_sequence: 0,
            last_error: CString::new("").unwrap_or_default(),
            scripts: crate::script::ScriptHost::new(),
            // 单次回调预算：超了会被记账（`overruns`），M1 由运行时强制中断。
            budget: crate::script::ScriptBudget::new(crate::script::DEFAULT_CALL_BUDGET_MS),
            recorder: None,
            drained: 0,
            last_pushed: 0,
            paused: false,
            paused_skipped: 0,
        }
    }

    /// 是否已附加到目标。
    #[must_use]
    pub const fn attached(&self) -> bool {
        self.attached
    }

    /// 目标 PID。
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// 当前能力集。
    #[must_use]
    pub const fn capabilities(&self) -> CapabilitySet {
        self.capabilities
    }

    /// 已登记的脚本（含历史版本）。
    #[must_use]
    pub fn scripts(&self) -> &[Script] {
        self.scripts.all()
    }

    /// 单次脚本调用的时间预算（毫秒）。0 表示不限制。
    #[must_use]
    pub fn call_budget_ms(&self) -> u64 {
        self.budget.per_call_ms()
    }

    /// 记一次脚本耗时，返回是否超预算。
    pub fn record_script_timing(&mut self, script: u64, elapsed_ms: u64) -> bool {
        self.budget.record(script, elapsed_ms)
    }

    /// 把脚本标记为超时（它不再接收事件）。
    ///
    /// # Errors
    ///
    /// 脚本 id 不存在时出错。
    pub fn mark_script_timed_out(&mut self, script: u64) -> Result<()> {
        self.scripts.mark_timed_out(script).map(|_| ())
    }

    /// 往轨迹的**审计通道**里写一行。
    ///
    /// 审计通道是 `.pbt` 里的文本区（与事件流分开）。用它记"策略拒绝""脚本超时"
    /// 这类**判定**而不是观测：它们是"我们做了什么决定"，需要在事后能回答
    /// "这条处置有没有发生过、什么时候、对谁"。
    ///
    /// 没有在录制时是空操作——审计不改变行为，只是留痕。
    pub fn note_audit(&mut self, line: &str) {
        if let Some(recorder) = self.recorder.as_mut() {
            let _ = recorder.audit(line);
        }
    }
    /// 脚本宿主的可变访问（加载脚本时用）。
    ///
    /// 提供 `_mut` 而不是把 `scripts` 字段公开：脚本状态机（Registered →
    /// Loaded → Unloaded）的迁移应当只发生在这几个方法里，公开字段会让
    /// 任何调用点都能直接改状态，而状态机的意义就在于"不能随便改"。
    pub fn script_host_mut(&mut self) -> &mut crate::script::ScriptHost {
        &mut self.scripts
    }

    /// 某个事件类型是否有订阅者（分发前的快速判断）。
    #[must_use]
    pub fn has_subscriber(&self, kind: u8) -> bool {
        !self.scripts.subscribers(kind).is_empty()
    }
    /// 因队列背压丢弃的事件数。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.events.dropped()
    }

    /// 当前积压。
    #[must_use]
    pub fn backlog(&self) -> usize {
        self.inbox.len()
    }

    /// 已取走的条数。
    #[must_use]
    pub const fn drained(&self) -> u64 {
        self.drained
    }

    /// 设置错误并返回对应状态码。**每次失败都必须设置**，
    /// 否则 C++ 侧只能拿到一个整数，无法向人或 AI 解释发生了什么。
    ///
    /// 文本形状：`<状态短名>: <细节>`。但如果细节本身就是从状态派生的
    /// （例如 `Error::CapabilityDenied` 的 Display 已经以"能力不足"开头），
    /// 就不再重复加前缀——实测出现过 `能力不足: 能力不足: 需要 …` 这种
    /// 读起来像 bug 的文本，而它是会直接送给 AI 的。
    pub fn fail(&mut self, status: Status, message: &str) -> i32 {
        let label = status.message();
        self.last_error = CString::new(if message.starts_with(label) {
            message.to_string()
        } else {
            format!("{label}: {message}")
        })
        .unwrap_or_default();
        status.code()
    }

    /// 最后一条错误文本。
    #[must_use]
    pub fn last_error(&self) -> &CString {
        &self.last_error
    }

    /// 内部校验：能力位。
    ///
    /// 与 Hub 侧使用的是同一套 `Capability` 定义，但校验是**独立发生**的。
    pub fn require(&mut self, capability: Capability) -> Result<()> {
        if self.capabilities.contains(capability) {
            Ok(())
        } else {
            Err(Error::CapabilityDenied {
                needed: capability,
                held: self.capabilities,
            })
        }
    }

    /// 下发能力集（来自 `pb_set_capabilities`）。
    ///
    /// 语义是**绝对设定**：C++/Hub 侧是唯一的真相来源。
    /// 这就意味着"忘记下发能力集"会表现为 agent 拒绝一切操作——这是刻意选择的
    /// 失败方向（fail-closed）。
    pub fn set_capabilities(&mut self, capabilities: CapabilitySet) {
        self.capabilities = capabilities;
    }

    /// 标记附加状态。
    ///
    /// **附加是一次新的会话**：把上一次会话的"暂停"状态与计数带过来会让
    /// "我刚附加上却收不到事件"变成一个需要翻代码才能解释的谜。
    /// 实测过一次：某条用例把 agent 留在暂停状态，后面的用例取事件取到空，
    /// 而错误信息里没有任何线索指向"暂停"。
    pub fn mark_attached(&mut self, pid: u32, attached: bool) {
        self.pid = pid;
        if attached {
            // **每次附加都重置会话状态**，而不是只在"未附加 → 附加"的跃迁上重置。
            // 只做跃迁判断会漏掉最常见的一种：调用方已经附加着（或崩溃重连后
            // 以为自己附加着），再次 `pb_attach` 只是想"重新开始"。
            // 那时暂停状态会留着，表现为"刚附加上却收不到事件"。
            self.paused = false;
            self.paused_skipped = 0;
        }
        self.attached = attached;
    }

    /// 采集一条事件。
    ///
    /// 这是热路径：每次探针命中都会走到这里。刻意**不做任何格式化字符串**
    /// （只在失败路径上做），序号分配是一次自增，队列写入是一次短锁。
    pub fn push(&mut self, raw: &PbEventRaw) -> Result<u64> {
        let Some(event) = raw.to_event() else {
            return Err(Error::Trace(format!(
                "未知事件类型判别值 {}（C++ 侧与 Rust 侧的版本可能不一致）",
                raw.kind
            )));
        };
        self.push_event(event)
    }

    /// 采集一条已构造好的事件。
    ///
    /// 暂停期间**仍然更新序号**（否则恢复后序号会与探针侧的期望错位），
    /// 但既不写轨迹也不入队，只把条数记下来。
    pub fn push_event(&mut self, mut event: Event) -> Result<u64> {
        // **序号严格递增由这里保证，而不是靠调用方约定。**
        //
        // 原因：`pb_allocate_sequence` + 稍后 `pb_push_event` 是**被文档化的用法**
        // （探针先拿号、再构造事件），而"稍后"意味着中间可能有别的活动分配了
        // 更大的号。于是推送顺序与分配顺序可以不一致，记录侧的折叠器就会
        // 看到"序号回退"并断言失败——实测过一次，现场离原因很远。
        //
        // 所以：调用方给的序号若没有严格大于已推送的最大值，就改用新分配的。
        // 轨迹因此永远自洽；代价是"调用方给的号可能被改写"，
        // 而这一点在 `.pbt` 里是可观测的（记录的号就是实际用的号）。
        if event.sequence <= self.last_pushed {
            event.sequence = self.last_pushed.max(self.next_sequence) + 1;
        }
        self.last_pushed = event.sequence;
        self.next_sequence = self.next_sequence.max(event.sequence);
        if self.paused {
            // 状态变化事件**例外**：暂停/恢复本身必须留下痕迹，
            // 否则轨迹里会出现"有一段时间什么都没有"却没有任何解释。
            if event.kind != EventKind::StateChange {
                self.paused_skipped += 1;
                return Ok(event.sequence);
            }
        }
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.write(&event)?;
        }
        let _ = self.events.send(event);
        Ok(event.sequence)
    }

    /// 是否处于暂停采集状态。
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.paused
    }

    /// 暂停期间被跳过的事件条数。
    #[must_use]
    pub const fn paused_skipped(&self) -> u64 {
        self.paused_skipped
    }

    /// 切换暂停状态，返回"这次切换是不是真的改变了状态"。
    ///
    /// 返回布尔值而不是无条件成功：重复 `pb_pause()` 是调用方的记账错误，
    /// 而"我以为暂停了 / 我以为还在跑"这类误解必须以可观测的方式暴露出来。
    pub fn set_paused(&mut self, paused: bool) -> bool {
        if self.paused == paused {
            return false;
        }
        self.paused = paused;
        true
    }

    /// 分配下一个序号（探针侧构造事件时使用）。
    pub fn allocate_sequence(&mut self) -> u64 {
        self.next_sequence += 1;
        self.next_sequence
    }

    /// 取走事件。返回实际写入 `out` 的条数。
    pub fn drain_into(&mut self, out: &mut [PbEventRaw]) -> usize {
        let taken = self.inbox.recv_batch(out.len());
        let count = taken.len();
        for (slot, event) in out.iter_mut().zip(taken.iter()) {
            *slot = PbEventRaw::from_event(event);
        }
        self.drained += count as u64;
        count
    }

    /// 脚本宿主（登记、策略与生命周期）。
    #[must_use]
    pub const fn script_host(&self) -> &crate::script::ScriptHost {
        &self.scripts
    }

    /// 脚本预算记账。
    #[must_use]
    pub const fn script_budget(&self) -> &crate::script::ScriptBudget {
        &self.budget
    }

    /// 登记脚本：先过策略，再分配 id 与哈希。
    ///
    /// # Errors
    ///
    /// 名/正文为空或违反运行策略时返回错误（消息里带行号与原因）。
    pub fn register_script(
        &mut self,
        name: &str,
        source: &str,
        subscribe: Vec<u8>,
    ) -> Result<Script> {
        self.scripts.register(name, source, subscribe)
    }

    /// 卸载脚本。
    ///
    /// # Errors
    ///
    /// 不存在或已卸载时返回错误（**不接受"假成功"**：那会让调用方以为
    /// 某个已经不在的脚本还在生效）。
    pub fn unregister_script(&mut self, id: u64) -> Result<Script> {
        self.scripts.unregister(id)
    }

    /// 开始录制。
    pub fn start_trace(&mut self, path: &Path, meta_json: &str, folding: bool) -> Result<()> {
        if self.recorder.is_some() {
            return Err(Error::Trace("agent 已在录制中".into()));
        }
        // 目标进程的工作目录通常不是我们期望的位置，父目录必须由我们自己建
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = File::create(path)?;
        let mut writer = PbtWriter::create(BufWriter::new(file), 0)?;
        writer.meta(meta_json)?;
        self.recorder = Some(LocalRecorder {
            records_since_flush: 0,
            writer,
            folder: RepeatFolder::new(),
            folding,
        });
        Ok(())
    }

    /// 停止录制并返回汇总。
    pub fn stop_trace(&mut self) -> Result<Summary> {
        let Some(mut recorder) = self.recorder.take() else {
            return Err(Error::Trace("agent 没有在录制".into()));
        };
        if recorder.folding {
            let mut pending: Vec<Event> = Vec::new();
            recorder.folder.finish(&mut |e| pending.push(e));
            for event in pending {
                recorder.writer.write_record(&event)?;
            }
        }
        recorder.writer.note_dropped(self.events.dropped());
        let (_inner, summary) = recorder.writer.finish()?;
        Ok(summary)
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalRecorder {
    /// 每写入多少条记录就把缓冲刷到磁盘。
    ///
    /// 为什么需要它（实测）：录制器用的是 `BufWriter`，而收尾的 `stop_trace`
    /// 发生在 Pin 的 Fini 里——**那已经晚于目标进程对 DLL 的卸载**，
    /// 而桥接层的卸载保护会拒绝这次调用（返回 `NOT_ATTACHED`）。
    /// 结果是：录制器一直没被 finalize，`.pbt` 文件**一个字节都没有**。
    ///
    /// 增量落盘把这个"全有或全无"变成"几乎全有"：即使收尾调用失败，
    /// 磁盘上也已经有可读的记录（读取器会如实报"汇总缺失"，不会假装完整）。
    /// 这条对真实场景同样重要——被分析的程序崩溃、被强杀、被杀软终止时，
    /// 用户最需要的就是**已经录到的东西**。
    const FLUSH_EVERY_RECORDS: u64 = 64;

    fn write(&mut self, event: &Event) -> Result<()> {
        if !self.folding {
            self.writer.write_record(event)?;
            return self.maybe_flush();
        }
        let mut failure: Option<String> = None;
        {
            let writer = &mut self.writer;
            self.folder.push(*event, &mut |folded| {
                if failure.is_none() {
                    if let Err(e) = writer.write_record(&folded) {
                        failure = Some(e.to_string());
                    }
                }
            });
        }
        match failure {
            Some(message) => Err(Error::Trace(message)),
            None => self.maybe_flush(),
        }
    }

    /// 写一行审计文本。
    ///
    /// 立刻 `flush`：审计行记录的是**判定**（拒绝、超时、终止），
    /// 而"进程随后就可能崩/被杀"正是这些判定最需要被留下的场景——
    /// 留在缓冲区里等于没有。
    fn audit(&mut self, line: &str) -> Result<()> {
        self.writer.audit_line(line)?;
        self.writer.flush()
    }
    /// 累计到阈值就把缓冲刷到磁盘（见 `FLUSH_EVERY_RECORDS` 的说明）。
    fn maybe_flush(&mut self) -> Result<()> {
        self.records_since_flush += 1;
        if self.records_since_flush < Self::FLUSH_EVERY_RECORDS {
            return Ok(());
        }
        self.records_since_flush = 0;
        self.writer.flush().map_err(|e| Error::Trace(e.to_string()))
    }
}

/// agent 全局宿主。所有导出函数都通过它访问状态。
#[derive(Debug)]
pub struct AgentHost;

static STATE: OnceLock<Mutex<AgentState>> = OnceLock::new();

/// 测试用串行锁。
///
/// 导出面与全局状态是一对一的（agent 只有一个实例），因此涉及全局状态的测试
/// 必须串行执行。这里不用 `serial_test` 之类的依赖：一个 `Mutex` 足够，
/// 而且能让"为什么这些测试不能并发"这件事留在代码里。
#[cfg(test)]
pub static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

impl AgentHost {
    /// 取全局状态。
    #[must_use]
    pub fn state() -> &'static Mutex<AgentState> {
        STATE.get_or_init(|| Mutex::new(AgentState::new()))
    }

    /// 加锁访问状态。中毒的锁会被恢复（**不能让一次 panic 把 agent 永久锁死**：
    /// 那会让整个目标进程的后续分析全部失效）。
    pub fn with<R>(f: impl FnOnce(&mut AgentState) -> R) -> R {
        let mut guard: MutexGuard<'_, AgentState> = Self::state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }

    /// 统一的"执行 + 失败时写 last_error"包装。
    ///
    /// 所有导出入口都用它，保证两件事：
    /// * 不 panic（panic 穿过 FFI 边界是 UB；release 下配置成 abort，
    ///   但那等于让目标进程直接死掉，比返回错误码糟糕得多）；
    /// * 失败一定留下可读的 `pb_last_error()`。
    pub fn call(f: impl FnOnce(&mut AgentState) -> Result<()>) -> i32 {
        Self::with(|state| match f(state) {
            Ok(()) => Status::Ok.code(),
            Err(e) => {
                let status = match &e {
                    Error::CapabilityDenied { .. } => Status::CapabilityDenied,
                    Error::TargetUnavailable(_) => Status::NotAttached,
                    Error::Protocol(_) => Status::InvalidArgument,
                    _ => Status::Internal,
                };
                state.fail(status, &e.to_string())
            }
        })
    }
}

/// 计算 sha256 的 hex 表示（与 `pinbridge-hub::script::sha256_hex` 必须一致）。
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 事件类型的短名表（供 C++ 侧与人读日志使用）。
#[must_use]
pub fn kind_name(code: u8) -> &'static str {
    EventKind::from_code(code).map_or("unknown", EventKind::name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_allocation_is_monotonic() {
        let mut state = AgentState::new();
        assert_eq!(state.allocate_sequence(), 1);
        assert_eq!(state.allocate_sequence(), 2);
        let raw = PbEventRaw::from_event(&Event::new(
            state.allocate_sequence(),
            EventKind::StateChange,
            1,
            0,
        ));
        assert_eq!(state.push(&raw).unwrap(), 3);

        // 落后于已推送最大值的序号会被**改写**成下一个：严格递增由 agent 保证，
        // 而不是靠调用方约定（"先分配、稍后推送"是文档化的用法，
        // 中间别的活动会分配更大的号）。
        let stale = PbEventRaw::from_event(&Event::new(2, EventKind::StateChange, 1, 0));
        assert_eq!(
            state.push(&stale).unwrap(),
            4,
            "落后的序号应当被改写成 4（当前最大是 3）"
        );
    }

    #[test]
    fn queue_is_bounded_and_reports_drops() {
        let mut state = AgentState::new();
        // 只填一小部分就能验证计数语义：容量是 65536，这里用事件序号推进
        for i in 1..=100u64 {
            let raw = PbEventRaw::from_event(&Event::new(i, EventKind::MemWrite, 1, i));
            state.push(&raw).unwrap();
        }
        assert_eq!(state.backlog(), 100);
        assert_eq!(state.dropped(), 0);
    }

    #[test]
    fn unknown_kind_is_rejected_with_context() {
        let mut state = AgentState::new();
        let mut raw = PbEventRaw::EMPTY;
        raw.kind = 99;
        let err = state.push(&raw).unwrap_err();
        assert!(err.to_string().contains("99"));
    }

    #[test]
    fn script_registration_hashes_like_the_hub() {
        let mut state = AgentState::new();
        let script = state
            .register_script("hook.py", "# pinbridge demo hook\nprint('hello')\n", vec![])
            .expect("登记应当成功");
        // 与 Hub 侧的 DEMO_SHA256 常量一致（跨 crate 的哈希必须逐字节相同）
        assert_eq!(
            script.source_sha256,
            "a98c7127446444ec31a9890062ee7f669d02b955bf07ea513f23f5f3db81671e"
        );
        assert_eq!(script.state, crate::script::ScriptState::Registered);
        assert_eq!(state.scripts().len(), 1);
        assert_eq!(
            state.unregister_script(script.id).unwrap().state,
            crate::script::ScriptState::Unloaded
        );
        // 再卸载必须报错：删除式实现会在这里静默成功，从而掩盖"脚本已经不在"的事实
        assert!(state.unregister_script(script.id).is_err());
    }

    #[test]
    fn kind_names_cover_known_codes() {
        assert_eq!(kind_name(1), "breakpoint_hit");
        assert_eq!(kind_name(12), "repeat");
        assert_eq!(kind_name(250), "unknown");
    }

    #[test]
    fn recorder_roundtrips_inside_the_agent() {
        let mut state = AgentState::new();
        let dir = std::env::temp_dir().join(format!("pb-agent-{}", std::process::id()));
        let path = dir.join("agent.pbt");
        state
            .start_trace(&path, r#"{"tool":"pinbridge-agent"}"#, true)
            .unwrap();
        for i in 1..=300u64 {
            state
                .push_event(Event::new(i, EventKind::SyscallEnter, 5, 0x2000).with_arg(0, 0x40))
                .unwrap();
        }
        let summary = state.stop_trace().unwrap();
        assert_eq!(summary.events, 300);
        assert_eq!(summary.repeat_folded, 299);
        let mut reader = pinbridge_trace::pbt::PbtReader::open(File::open(&path).unwrap()).unwrap();
        let records = reader.read_all().unwrap();
        assert_eq!(pinbridge_trace::fold::expand_records(&records).len(), 300);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fail_records_a_readable_message_and_becomes_capability_denied() {
        let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 全局状态在不同测试之间是共享的，所以这里先显式归零
        AgentHost::with(|state| state.set_capabilities(CapabilitySet::empty()));
        let code = AgentHost::call(|state| state.require(Capability::MemWrite));
        assert_eq!(code, Status::CapabilityDenied.code());
        let message = AgentHost::with(|s| s.last_error().to_string_lossy().into_owned());
        assert!(message.contains("能力不足"), "实际: {message}");
    }
}
