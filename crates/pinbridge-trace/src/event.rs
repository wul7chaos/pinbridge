//! 事件模型。
//!
//! 事件是 PinBridge 里唯一的"事实单位"。设计上做了两个取舍：
//!
//! 1. **定长参数数组 `args: [u64; 8]`**，而不是 `Vec<u64>`。理由：agent 在目标
//!    进程的探针上下文里构造事件，堆分配在那种上下文里既慢又危险（可能重入
//!    分配器）。8 个参数覆盖了断点、异常、syscall、指令模板这几类时机的实际需要。
//! 2. **序号是唯一的顺序依据**（不是时间戳）。tick 精度在不同机器上差异巨大，
//!    而序号在录制时单调递增，重放时可以精确还原顺序，也让"折叠重复段"成为
//!    一个纯算术判断。

/// 事件参数个数。改动它等于改协议，必须走版本流程。
pub const MAX_ARGS: usize = 8;

/// 事件类型。判别值写入文件，**不得重排**；新增只能追加并更新
/// `docs/specs/pbt-format.md` 的类型表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EventKind {
    /// 保留值，遇到 `0` 视为文件损坏。
    Reserved = 0,
    /// 命中断点。args[0] = 断点地址。
    BreakpointHit = 1,
    /// Windows 异常时机。args[0] = 异常码，args[1] = 异常地址，args[2] = 是否继续搜索。
    Exception = 2,
    /// 系统调用进入。args[0] = 调用号，args[1..=4] = 前四个参数。
    SyscallEnter = 3,
    /// 系统调用返回。args[0] = 调用号，args[1] = 返回值。
    SyscallExit = 4,
    /// 指令模板命中。args[0] = 模板 ID。
    PatternHit = 5,
    /// 内存写入。args[0] = 地址，args[1] = 长度，args[2] = 新值（<=8 字节时）。
    MemWrite = 6,
    /// 内存读取（需显式开启，开销大）。
    MemRead = 7,
    /// 寄存器被改写。args[0] = 寄存器编号，args[1] = 新值。
    RegWrite = 8,
    /// TF（Trap Flag）被目标显式设置 —— 对应 `docs/design/flags-virtualization.md`。
    TrapFlag = 9,
    /// 执行上下文状态变化（暂停/继续/脚本切换）。
    StateChange = 10,
    /// 脚本产生的输出（打印/判定结果）。
    ScriptOutput = 11,
    /// 折叠标记。**只出现在轨迹文件中**，agent 的原始事件流里永远不会产生它。
    Repeat = 12,
}

impl EventKind {
    /// 判别值。
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// 由判别值还原。未知值返回 `None`（读者的策略是拒绝而不是猜测：
    /// 把未知类型当成 `Reserved` 会让"文件来自新版本"这种正常情况变成静默错读）。
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::BreakpointHit,
            2 => Self::Exception,
            3 => Self::SyscallEnter,
            4 => Self::SyscallExit,
            5 => Self::PatternHit,
            6 => Self::MemWrite,
            7 => Self::MemRead,
            8 => Self::RegWrite,
            9 => Self::TrapFlag,
            10 => Self::StateChange,
            11 => Self::ScriptOutput,
            12 => Self::Repeat,
            _ => return None,
        })
    }

    /// 短名，用于 CLI 输出与 JSON 视图。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::BreakpointHit => "breakpoint_hit",
            Self::Exception => "exception",
            Self::SyscallEnter => "syscall_enter",
            Self::SyscallExit => "syscall_exit",
            Self::PatternHit => "pattern_hit",
            Self::MemWrite => "mem_write",
            Self::MemRead => "mem_read",
            Self::RegWrite => "reg_write",
            Self::TrapFlag => "trap_flag",
            Self::StateChange => "state_change",
            Self::ScriptOutput => "script_output",
            Self::Repeat => "repeat",
        }
    }

    /// 该类型在默认配置下是否默认订阅。`MemRead` 默认关闭：
    /// 一次成功的 memcpy 就能产生几十万条读事件，会把真正有用的信号淹掉。
    #[must_use]
    pub const fn default_subscribed(self) -> bool {
        !matches!(self, Self::MemRead | Self::Reserved)
    }
}

/// 一条事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// 全局单调序号，从 1 开始（0 保留给"未初始化"）。
    pub sequence: u64,
    /// 类型。
    pub kind: EventKind,
    /// 线程 ID。
    pub thread_id: u32,
    /// 主地址（断点地址 / 指令地址 / 被访问地址）。
    pub address: u64,
    /// 定长参数。
    pub args: [u64; MAX_ARGS],
}

impl Event {
    /// 空事件（`sequence = 0`，其余为 0）。与文章中的 `Event::EMPTY` 对应。
    pub const EMPTY: Self = Self {
        sequence: 0,
        kind: EventKind::Reserved,
        thread_id: 0,
        address: 0,
        args: [0; MAX_ARGS],
    };

    /// 构造一条普通事件。
    #[must_use]
    pub const fn new(sequence: u64, kind: EventKind, thread_id: u32, address: u64) -> Self {
        Self {
            sequence,
            kind,
            thread_id,
            address,
            args: [0; MAX_ARGS],
        }
    }

    /// 链式设置参数。
    #[must_use]
    pub fn with_arg(mut self, index: usize, value: u64) -> Self {
        if index < MAX_ARGS {
            self.args[index] = value;
        }
        self
    }

    /// 读取参数。
    #[must_use]
    pub const fn arg(&self, index: usize) -> u64 {
        if index < MAX_ARGS {
            self.args[index]
        } else {
            0
        }
    }

    /// 载荷比较：**只比较类型、线程、地址与 args[0..8]，不比较序号**。
    ///
    /// 这是折叠算法的核心判定（文章里的 `same_payload`）。不比较序号是刻意的：
    /// 序号正是我们要折叠掉的信息——同一条指令重复执行 100 万次时，
    /// 只有序号在变，其余完全相同。
    #[must_use]
    pub fn same_payload(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.thread_id == other.thread_id
            && self.address == other.address
            && self.args == other.args
    }

    /// 构造折叠标记记录。
    ///
    /// 语义（与 `docs/design/trace-compression.md` 的 §折叠语义 完全一致）：
    /// * `sequence` = 该重复段**最后一条**事件的序号；
    /// * `args[0]` = 被折叠掉的条数（不含第一次出现的那条）；
    /// * `args[1]` = 重复段的原始事件类型；
    /// * `thread_id` / `address` 取自该重复段。
    #[must_use]
    pub fn repeat_marker(
        last_sequence: u64,
        folded_count: u64,
        kind: EventKind,
        sample: &Self,
    ) -> Self {
        Self::new(
            last_sequence,
            EventKind::Repeat,
            sample.thread_id,
            sample.address,
        )
        .with_arg(0, folded_count)
        .with_arg(1, u64::from(kind.code()))
    }

    /// 事件编码成可读的文本行（调试用，不用于落盘）。
    #[must_use]
    pub fn to_text(&self) -> String {
        format!(
            "#{} {} tid={} addr=0x{:X} args=[{}]",
            self.sequence,
            self.kind.name(),
            self.thread_id,
            self.address,
            self.args
                .iter()
                .map(|v| format!("0x{v:X}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_codes_roundtrip_and_are_stable() {
        for code in 1..=12u8 {
            let kind = EventKind::from_code(code).expect("已知类型");
            assert_eq!(kind.code(), code);
            assert!(!kind.name().is_empty());
        }
        assert_eq!(EventKind::Reserved.code(), 0);
        assert!(EventKind::from_code(0).is_none(), "0 是保留值");
        assert!(EventKind::from_code(200).is_none(), "未知类型必须返回 None");
    }

    #[test]
    fn same_payload_ignores_sequence() {
        let a = Event::new(10, EventKind::SyscallEnter, 100, 0x1000).with_arg(0, 0x40);
        let b = Event::new(11, EventKind::SyscallEnter, 100, 0x1000).with_arg(0, 0x40);
        assert!(a.same_payload(&b));

        let mut c = b;
        c.thread_id = 101;
        assert!(!a.same_payload(&c), "不同线程不算重复");
        let mut d = b;
        d.address = 0x1001;
        assert!(!a.same_payload(&d), "不同地址不算重复");
        let mut e = b;
        e.args[0] = 0x41;
        assert!(!a.same_payload(&e), "不同参数不算重复");
    }

    #[test]
    fn repeat_marker_carries_original_kind_and_count() {
        let sample = Event::new(10, EventKind::BreakpointHit, 7, 0xDEAD_BEEF);
        let marker = Event::repeat_marker(30, 20, EventKind::BreakpointHit, &sample);
        assert_eq!(marker.kind, EventKind::Repeat);
        assert_eq!(marker.sequence, 30);
        assert_eq!(marker.arg(0), 20);
        assert_eq!(marker.arg(1), u64::from(EventKind::BreakpointHit.code()));
        assert_eq!(marker.thread_id, 7);
        assert_eq!(marker.address, 0xDEAD_BEEF);
    }

    #[test]
    fn arg_access_is_bounds_checked() {
        let ev = Event::EMPTY.with_arg(99, 5);
        assert_eq!(ev.arg(0), 0);
        assert_eq!(ev.arg(MAX_ARGS + 5), 0);
    }

    #[test]
    fn mem_read_is_not_default_subscribed() {
        assert!(!EventKind::MemRead.default_subscribed());
        assert!(EventKind::BreakpointHit.default_subscribed());
        assert!(EventKind::Repeat.default_subscribed());
    }

    #[test]
    fn text_view_mentions_kind_and_sequence() {
        let ev = Event::new(3, EventKind::MemWrite, 1, 0x4000_0000);
        let text = ev.to_text();
        assert!(text.starts_with("#3 mem_write"), "实际: {text}");
        assert!(text.contains("addr=0x40000000"));
    }
}
