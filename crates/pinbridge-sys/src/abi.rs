//! C ABI 数据类型与状态码。
//!
//! 这个文件是 **C++ 侧 `native/include/pinbridge.h` 的 Rust 孪生体**。
//! 两份必须逐字段一致；改动规则见 `docs/specs/c-abi.md` §变更流程：
//! 任何字段的增删都要同时改两侧、提升 `PB_ABI_VERSION`、并更新规范文档。
//!
//! 刻意不使用 `#[repr(Rust)]` 或 `bool`：
//! `bool` 在 C 侧是 1 字节、在 C++ 侧也是 1 字节，但跨编译器/跨语言的
//! 布局假设是最经典的一类 ABI 事故来源。这里**只出现定长整数、指针与定长数组**。

use std::os::raw::c_char;

use pinbridge_core::capability::CapabilitySet;
use pinbridge_core::error::{Error, Result};
use pinbridge_trace::event::{Event, EventKind};

/// 当前 ABI 版本。C++ 侧 `pb_abi_version()` 必须返回同一个值，
/// 否则加载器会拒绝加载——**版本不匹配时拒绝加载，而不是尽力而为**。
pub const PB_ABI_VERSION: u32 = 1;

/// 单条事件的 C 布局。
///
/// 与 `pinbridge_trace::event::Event` 一一对应，只是把 `EventKind` 降级成 `u8`
/// 判别值（C 侧不认识 Rust 枚举）。判别值表见 `docs/specs/pbt-format.md`。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PbEventRaw {
    /// 全局单调序号。
    pub sequence: u64,
    /// 事件类型判别值。
    pub kind: u8,
    /// 显式填充：让 `thread_id` 落在 4 字节边界上。
    /// 不用隐式填充，因为不同编译器对隐式填充的处理在跨语言边界上并不可靠。
    pub reserved: [u8; 3],
    /// 线程 ID。
    pub thread_id: u32,
    /// 主地址。
    pub address: u64,
    /// 定长参数。
    pub args: [u64; 8],
}

impl PbEventRaw {
    /// 空事件。
    pub const EMPTY: Self = Self {
        sequence: 0,
        kind: 0,
        reserved: [0; 3],
        thread_id: 0,
        address: 0,
        args: [0; 8],
    };

    /// 转换为 Rust 事件。判别值非法时返回 `None`
    /// （非法判别值意味着 C++ 侧与 Rust 侧的版本已经不一致，必须显式失败）。
    #[must_use]
    pub fn to_event(&self) -> Option<Event> {
        let kind = EventKind::from_code(self.kind)?;
        let mut event = Event::new(self.sequence, kind, self.thread_id, self.address);
        for (index, value) in self.args.iter().enumerate() {
            event = event.with_arg(index, *value);
        }
        Some(event)
    }

    /// 由 Rust 事件构造 C 布局。
    #[must_use]
    pub fn from_event(event: &Event) -> Self {
        Self {
            sequence: event.sequence,
            kind: event.kind.code(),
            reserved: [0; 3],
            thread_id: event.thread_id,
            address: event.address,
            args: event.args,
        }
    }
}

/// 能力集的 C 表示。与 Rust 侧的位图完全一致。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PbSet(pub u32);

impl From<CapabilitySet> for PbSet {
    fn from(value: CapabilitySet) -> Self {
        Self(value.bits())
    }
}

impl From<PbSet> for CapabilitySet {
    fn from(value: PbSet) -> Self {
        Self::from_bits(value.0)
    }
}

/// C 侧状态码。**数值一旦发布就不可更改**（C 侧可能已经按数值分支）。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PbStatus {
    /// 成功。
    Ok = 0,
    /// 参数非法（空指针、长度越界等）。
    InvalidArgument = -1,
    /// 尚未附加到目标。
    NotAttached = -2,
    /// 能力不足。
    CapabilityDenied = -3,
    /// 当前平台/Pin 版本不支持该操作。
    NotSupported = -4,
    /// 目标正忙（例如正在被另一个探针持有）。
    Busy = -5,
    /// 超时。
    Timeout = -6,
    /// 内部错误（不可恢复状态或已捕获的 panic）。
    Internal = -7,
    /// ABI 版本不匹配。
    AbiMismatch = -8,
}

/// Rust 侧的状态枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 成功。
    Ok,
    /// 参数非法。
    InvalidArgument,
    /// 未附加。
    NotAttached,
    /// 能力不足。
    CapabilityDenied,
    /// 不支持。
    NotSupported,
    /// 忙。
    Busy,
    /// 超时。
    Timeout,
    /// 内部错误。
    Internal,
    /// ABI 不匹配。
    AbiMismatch,
}

impl Status {
    /// 转成 C 状态码。
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::InvalidArgument => -1,
            Self::NotAttached => -2,
            Self::CapabilityDenied => -3,
            Self::NotSupported => -4,
            Self::Busy => -5,
            Self::Timeout => -6,
            Self::Internal => -7,
            Self::AbiMismatch => -8,
        }
    }

    /// 从 C 状态码还原。未知码返回 `None`（不猜）。
    #[must_use]
    pub const fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Ok,
            -1 => Self::InvalidArgument,
            -2 => Self::NotAttached,
            -3 => Self::CapabilityDenied,
            -4 => Self::NotSupported,
            -5 => Self::Busy,
            -6 => Self::Timeout,
            -7 => Self::Internal,
            -8 => Self::AbiMismatch,
            _ => return None,
        })
    }

    /// 人类可读描述。
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::InvalidArgument => "参数非法",
            Self::NotAttached => "尚未附加到目标进程",
            Self::CapabilityDenied => "能力不足",
            Self::NotSupported => "当前平台或 Pin 版本不支持该操作",
            Self::Busy => "目标正忙",
            Self::Timeout => "操作超时",
            Self::Internal => "内部错误",
            Self::AbiMismatch => "ABI 版本不匹配",
        }
    }

    /// 映射到项目统一错误类型，便于在上层用同一种方式处理。
    #[must_use]
    pub fn to_error(self, context: &str, held: CapabilitySet) -> Option<Error> {
        match self {
            Self::Ok => None,
            Self::NotAttached | Self::Busy | Self::Timeout => Some(Error::TargetUnavailable(
                format!("{context}: {}", self.message()),
            )),
            Self::CapabilityDenied => Some(Error::CapabilityDenied {
                needed: pinbridge_core::Capability::MemRead,
                held,
            }),
            Self::AbiMismatch | Self::InvalidArgument => {
                Some(Error::Protocol(format!("{context}: {}", self.message())))
            }
            _ => Some(Error::Trace(format!("{context}: {}", self.message()))),
        }
    }
}

/// 把 C 状态码转成 `Result`，非零时附带上下文。
///
/// `held` 用于把"能力不足"还原成可读的能力集（`Status::CapabilityDenied`
/// 在 ABI 上只是一个整数，上层需要自己补上上下文）。
pub fn check(code: i32, context: &str, held: CapabilitySet) -> Result<()> {
    match Status::from_code(code) {
        Some(Status::Ok) => Ok(()),
        Some(status) => Err(status
            .to_error(context, held)
            .unwrap_or_else(|| Error::Trace(format!("{context}: 未知状态码 {code}")))),
        None => Err(Error::Protocol(format!("{context}: 未知状态码 {code}"))),
    }
}

/// agent 导出函数的 ABI 版本检查。
#[must_use]
pub const fn abi_version_compatible(agent_reports: u32) -> bool {
    agent_reports == PB_ABI_VERSION
}

/// 读取只读 C 字符串的辅助函数。
///
/// # Safety
///
/// `ptr` 必须指向以 `\0` 结尾的有效 UTF-8 字节序列，或为 `NULL`。
/// 字符串必须在调用期间保持存活。
pub unsafe fn cstr_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: 调用方保证 ptr 以 NUL 结尾且在本调用期间存活
    let bytes = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes();
    std::str::from_utf8(bytes).ok().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_layout_is_explicit_and_stable() {
        // 88 = 8(seq) + 1(kind) + 3(pad) + 4(tid) + 8(addr) + 64(args)
        assert_eq!(std::mem::size_of::<PbEventRaw>(), 88);
        assert_eq!(std::mem::align_of::<PbEventRaw>(), 8);
        assert_eq!(std::mem::size_of::<PbSet>(), 4);
    }

    #[test]
    fn event_roundtrip_between_rust_and_c_layout() {
        let event = Event::new(42, EventKind::MemWrite, 7, 0x7FF6_0000_3000)
            .with_arg(0, 0xDEAD_BEEF)
            .with_arg(3, 4);
        let raw = PbEventRaw::from_event(&event);
        assert_eq!(raw.kind, EventKind::MemWrite.code());
        assert_eq!(raw.to_event(), Some(event));
    }

    #[test]
    fn unknown_kind_is_rejected_instead_of_guessed() {
        let mut raw = PbEventRaw::EMPTY;
        raw.kind = 200;
        assert!(
            raw.to_event().is_none(),
            "未知判别值必须失败，不能猜一个默认类型"
        );
    }

    #[test]
    fn status_codes_are_stable_and_roundtrip() {
        for status in [
            Status::Ok,
            Status::InvalidArgument,
            Status::NotAttached,
            Status::CapabilityDenied,
            Status::NotSupported,
            Status::Busy,
            Status::Timeout,
            Status::Internal,
            Status::AbiMismatch,
        ] {
            assert_eq!(Status::from_code(status.code()), Some(status));
            assert_eq!(status.code(), PbStatus::from_rust(status) as i32);
        }
        assert_eq!(Status::from_code(12345), None);
        assert!(check(0, "ctx", CapabilitySet::empty()).is_ok());
        assert!(check(-3, "ctx", CapabilitySet::empty()).is_err());
        assert!(check(77, "ctx", CapabilitySet::empty()).is_err());
    }

    #[test]
    fn capability_bitset_crosses_the_boundary_unchanged() {
        let caps = CapabilitySet::from_iter([
            pinbridge_core::Capability::MemRead,
            pinbridge_core::Capability::ScriptLoad,
        ]);
        let raw = PbSet::from(caps);
        assert_eq!(raw.0, caps.bits());
        assert_eq!(CapabilitySet::from(raw), caps);
    }

    #[test]
    fn abi_version_must_match_exactly() {
        assert!(abi_version_compatible(PB_ABI_VERSION));
        assert!(!abi_version_compatible(PB_ABI_VERSION + 1));
        assert!(!abi_version_compatible(0));
    }

    #[test]
    fn null_c_string_is_none_and_valid_utf8_is_decoded() {
        assert!(unsafe { cstr_to_string(std::ptr::null()) }.is_none());
        let text = std::ffi::CString::new("目标: TARGET.exe").unwrap();
        // SAFETY: CString 保证以 NUL 结尾且在本语句期间存活
        let decoded = unsafe { cstr_to_string(text.as_ptr()) }.unwrap();
        assert_eq!(decoded, "目标: TARGET.exe");
    }
}

/// 仅供测试与文档使用的映射：把 Rust `Status` 转成 `PbStatus` 的具名常量。
impl PbStatus {
    /// 由 Rust 状态取对应的 C 枚举常量（保证两侧数值一致）。
    #[must_use]
    pub const fn from_rust(status: Status) -> Self {
        match status {
            Status::Ok => Self::Ok,
            Status::InvalidArgument => Self::InvalidArgument,
            Status::NotAttached => Self::NotAttached,
            Status::CapabilityDenied => Self::CapabilityDenied,
            Status::NotSupported => Self::NotSupported,
            Status::Busy => Self::Busy,
            Status::Timeout => Self::Timeout,
            Status::Internal => Self::Internal,
            Status::AbiMismatch => Self::AbiMismatch,
        }
    }
}
