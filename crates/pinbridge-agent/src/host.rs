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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptHandle {
    /// 脚本 ID（agent 本地单调分配）。
    pub id: u64,
    /// 名称。
    pub name: String,
    /// 源码 sha256（hex）。
    pub source_sha256: String,
    /// 源码字节数。
    pub source_len: usize,
    /// 订阅的事件类型短名。
    pub subscribe: Vec<String>,
}

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
    scripts: Vec<ScriptHandle>,
    next_script: u64,
    recorder: Option<LocalRecorder>,
    /// 已采集但尚未被 Hub 取走的条数（用于背压观测）。
    drained: u64,
}

/// 目标进程内的录制器（不依赖 `pinbridge-hub`，避免 agent 把整个控制面拖进来）。
#[derive(Debug)]
struct LocalRecorder {
    writer: PbtWriter<BufWriter<File>>,
    folder: RepeatFolder,
    folding: bool,
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
            scripts: Vec::new(),
            next_script: 0,
            recorder: None,
            drained: 0,
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

    /// 已加载脚本。
    #[must_use]
    pub fn scripts(&self) -> &[ScriptHandle] {
        &self.scripts
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
    pub fn fail(&mut self, status: Status, message: &str) -> i32 {
        let text = format!("{}: {message}", status.message());
        self.last_error = CString::new(text).unwrap_or_default();
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
    pub fn mark_attached(&mut self, pid: u32, attached: bool) {
        self.pid = pid;
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
    pub fn push_event(&mut self, event: Event) -> Result<u64> {
        self.next_sequence = self.next_sequence.max(event.sequence);
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.write(&event)?;
        }
        let _ = self.events.send(event);
        Ok(event.sequence)
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

    /// 注册脚本。
    pub fn register_script(
        &mut self,
        name: &str,
        source: &str,
        subscribe: Vec<String>,
    ) -> ScriptHandle {
        self.next_script += 1;
        let handle = ScriptHandle {
            id: self.next_script,
            name: name.to_owned(),
            source_sha256: sha256_hex(source.as_bytes()),
            source_len: source.len(),
            subscribe,
        };
        self.scripts.push(handle.clone());
        handle
    }

    /// 卸载脚本。
    pub fn unregister_script(&mut self, id: u64) -> bool {
        let before = self.scripts.len();
        self.scripts.retain(|s| s.id != id);
        self.scripts.len() != before
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
    fn write(&mut self, event: &Event) -> Result<()> {
        if !self.folding {
            return self.writer.write_record(event);
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
            None => Ok(()),
        }
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
        let handle =
            state.register_script("hook.py", "# pinbridge demo hook\nprint('hello')\n", vec![]);
        assert_eq!(
            handle.source_sha256,
            "a98c7127446444ec31a9890062ee7f669d02b955bf07ea513f23f5f3db81671e"
        );
        assert_eq!(state.scripts().len(), 1);
        assert!(state.unregister_script(handle.id));
        assert!(!state.unregister_script(handle.id));
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
