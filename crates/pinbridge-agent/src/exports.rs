//! `pinbridge_agentdll` 的 C ABI 导出面。
//!
//! # 完整清单（与 `native/include/pinbridge.h` 一一对应）
//!
//! | 导出函数 | 所需能力 | M0 状态 |
//! |----------|----------|---------|
//! | `pb_abi_version` | — | 完整 |
//! | `pb_attach` / `pb_detach` | — | 完整（状态标记） |
//! | `pb_set_capabilities` / `pb_get_capabilities` | — | 完整 |
//! | `pb_last_error` | — | 完整 |
//! | `pb_allocate_sequence` / `pb_push_event` / `pb_drain_events` | — | 完整 |
//! | `pb_start_trace` / `pb_stop_trace` | `trace_record` | 完整 |
//! | `pb_memory_read` / `pb_memory_write` | `mem_read` / `mem_write` | 桩（`not_supported`） |
//! | `pb_set_breakpoint` | `breakpoint` | 桩 |
//! | `pb_single_step` / `pb_continue` / `pb_pause` | `single_step` / `process_control` | 桩 |
//! | `pb_load_script` | `script_load` | 桩（内嵌 CPython 见 M1） |
//!
//! # 桩函数的行为约定
//!
//! 桩函数**先做能力校验**，能力通过之后返回 `PB_STATUS_NOT_SUPPORTED`。
//! 顺序不能反：如果桩函数直接返回"不支持"，那么"能力校验是否真的生效"
//! 在 M0 阶段就无法被测试覆盖；等 M1 把桩换成真实现时，这条链路会第一次
//! 被真正执行——那是最不适合发现权限漏洞的时机。
//!
//! # 指针契约
//!
//! * 所有 `*const` / `*mut` 参数都允许为 `NULL`（返回 `PB_STATUS_INVALID_ARGUMENT`），
//!   除非注明"必需"；
//! * `pb_last_error` 返回的指针在**下一次修改错误状态之前**有效；
//!   调用方必须立刻拷贝，不得跨调用保存。

use std::os::raw::{c_char, c_int, c_uint};

use pinbridge_core::capability::Capability;
use pinbridge_core::error::Error;
use pinbridge_sys::abi::{cstr_to_string, PbEventRaw, Status, PB_ABI_VERSION};

use crate::host::{kind_name, AgentHost};

/// 探测 ABI 版本。**加载器必须先调它**，版本不符时拒绝加载。
#[unsafe(no_mangle)]
pub extern "C" fn pb_abi_version() -> u32 {
    PB_ABI_VERSION
}

/// 标记已附加到目标进程。
#[unsafe(no_mangle)]
pub extern "C" fn pb_attach(pid: c_uint) -> c_int {
    AgentHost::with(|state| {
        state.mark_attached(pid, true);
        Status::Ok.code()
    })
}

/// 解除附加状态（不清空能力集：能力由 Hub 显式管理）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_detach() -> c_int {
    AgentHost::with(|state| {
        state.mark_attached(0, false);
        Status::Ok.code()
    })
}

/// 是否已附加（1 = 是，0 = 否）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_is_attached() -> c_int {
    AgentHost::with(|state| c_int::from(state.attached()))
}

/// 下发能力集（位图）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_set_capabilities(bits: c_uint) -> c_int {
    AgentHost::with(|state| {
        state.set_capabilities(pinbridge_core::CapabilitySet::from_bits(bits));
        Status::Ok.code()
    })
}

/// 读回当前能力集。
#[unsafe(no_mangle)]
pub extern "C" fn pb_get_capabilities() -> c_uint {
    AgentHost::with(|state| state.capabilities().bits())
}

/// 取最后一条错误文本。返回的指针在下一次错误写入前有效。
#[unsafe(no_mangle)]
pub extern "C" fn pb_last_error() -> *const c_char {
    AgentHost::with(|state| state.last_error().as_ptr())
}

/// 分配下一个事件序号。探针侧构造事件时使用。
#[unsafe(no_mangle)]
pub extern "C" fn pb_allocate_sequence() -> u64 {
    AgentHost::with(crate::host::AgentState::allocate_sequence)
}

/// **热路径**：推送一条事件。
///
/// # Safety
///
/// `raw` 必须指向一个有效的 `PbEventRaw`，或为 `NULL`。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_push_event(raw: *const PbEventRaw) -> c_int {
    if raw.is_null() {
        return AgentHost::with(|state| {
            state.fail(Status::InvalidArgument, "pb_push_event: raw 为空指针")
        });
    }
    // SAFETY: 上面已判空；调用方保证指针在本次调用期间有效
    let event = unsafe { *raw };
    AgentHost::call(|state| state.push(&event).map(|_| ()))
}

/// 取走事件批次。
///
/// # Safety
///
/// `out` 必须是指向 `capacity` 个 `PbEventRaw` 的可写缓冲区（或 `NULL`）；
/// `written` 必须是指向 `u32` 的可写指针（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_drain_events(
    out: *mut PbEventRaw,
    capacity: c_uint,
    written: *mut c_uint,
) -> c_int {
    if out.is_null() || written.is_null() {
        return AgentHost::with(|state| {
            state.fail(
                Status::InvalidArgument,
                "pb_drain_events: out/written 为空指针",
            )
        });
    }
    if capacity == 0 {
        return AgentHost::with(|state| {
            state.fail(Status::InvalidArgument, "pb_drain_events: capacity 为 0")
        });
    }
    // SAFETY: 由调用方保证缓冲区大小与 capacity 一致
    let buffer = unsafe { std::slice::from_raw_parts_mut(out, capacity as usize) };
    let count = AgentHost::with(|state| state.drain_into(buffer));
    // SAFETY: written 已判空
    unsafe { *written = count as c_uint };
    Status::Ok.code()
}

/// 取回一个统计量：`which` 0=丢弃数 1=积压 2=已取走。
#[unsafe(no_mangle)]
pub extern "C" fn pb_stats(which: c_uint) -> u64 {
    AgentHost::with(|state| match which {
        0 => state.dropped(),
        1 => state.backlog() as u64,
        _ => state.drained(),
    })
}

/// 事件类型判别值 → 短名（永不返回 `NULL`）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_kind_name(code: c_uint) -> *const c_char {
    // 短名是 'static 字面量，NUL 结尾，可以直接给 C 用
    const NAMES: &[&str] = &[
        "reserved",
        "breakpoint_hit",
        "exception",
        "syscall_enter",
        "syscall_exit",
        "pattern_hit",
        "mem_write",
        "mem_read",
        "reg_write",
        "trap_flag",
        "state_change",
        "script_output",
        "repeat",
    ];
    let index = code as usize;
    let name = if index < NAMES.len() {
        NAMES[index]
    } else {
        kind_name(255)
    };
    // 手工拼一个 NUL 结尾的静态字符串
    match name {
        "reserved" => c"reserved".as_ptr(),
        "breakpoint_hit" => c"breakpoint_hit".as_ptr(),
        "exception" => c"exception".as_ptr(),
        "syscall_enter" => c"syscall_enter".as_ptr(),
        "syscall_exit" => c"syscall_exit".as_ptr(),
        "pattern_hit" => c"pattern_hit".as_ptr(),
        "mem_write" => c"mem_write".as_ptr(),
        "mem_read" => c"mem_read".as_ptr(),
        "reg_write" => c"reg_write".as_ptr(),
        "trap_flag" => c"trap_flag".as_ptr(),
        "state_change" => c"state_change".as_ptr(),
        "script_output" => c"script_output".as_ptr(),
        "repeat" => c"repeat".as_ptr(),
        _ => c"unknown".as_ptr(),
    }
}

/// 读目标内存（M0 桩）。
///
/// # Safety
///
/// `out` 必须是指向 `out_capacity` 字节的可写缓冲区（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_memory_read(
    _address: u64,
    length: c_uint,
    out: *mut u8,
    out_capacity: c_uint,
    out_written: *mut c_uint,
) -> c_int {
    if out.is_null() || out_written.is_null() {
        return AgentHost::with(|state| {
            state.fail(
                Status::InvalidArgument,
                "pb_memory_read: out/out_written 为空指针",
            )
        });
    }
    if length > out_capacity {
        return AgentHost::with(|state| {
            state.fail(
                Status::InvalidArgument,
                "pb_memory_read: length 超出缓冲区容量",
            )
        });
    }
    // SAFETY: out_written 已判空
    unsafe { *out_written = 0 };
    mapped(
        AgentHost::call(|state| {
            state.require(Capability::MemRead)?;
            Err(Error::Trace(
                "pb_memory_read: 需要 Pin 运行时（M0 桩实现，见 docs/roadmap.md 的 M1）".into(),
            ))
        }),
        Status::NotSupported,
    )
}

/// 写目标内存（M0 桩）。
///
/// # Safety
///
/// `bytes` 必须指向 `length` 字节的可读缓冲区（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_memory_write(_address: u64, bytes: *const u8, length: c_uint) -> c_int {
    if bytes.is_null() && length != 0 {
        return AgentHost::with(|state| {
            state.fail(Status::InvalidArgument, "pb_memory_write: bytes 为空指针")
        });
    }
    mapped(
        AgentHost::call(|state| {
            state.require(Capability::MemWrite)?;
            Err(Error::Trace(
                "pb_memory_write: 需要 Pin 运行时（M0 桩实现）".into(),
            ))
        }),
        Status::NotSupported,
    )
}

/// 下断点（M0 桩）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_set_breakpoint(_address: u64) -> c_int {
    stub(Capability::Breakpoint, "pb_set_breakpoint")
}

/// 单步（M0 桩）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_single_step(_count: c_uint) -> c_int {
    stub(Capability::SingleStep, "pb_single_step")
}

/// 继续执行（M0 桩）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_continue() -> c_int {
    stub(Capability::ProcessControl, "pb_continue")
}

/// 中断执行（M0 桩）。
#[unsafe(no_mangle)]
pub extern "C" fn pb_pause() -> c_int {
    stub(Capability::ProcessControl, "pb_pause")
}

/// 加载脚本（M0 桩：能力校验已生效，CPython 宿主见 M1）。
///
/// # Safety
///
/// `name` / `source` 必须是 NUL 结尾的有效 UTF-8 字符串（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_load_script(name: *const c_char, source: *const c_char) -> c_int {
    if name.is_null() || source.is_null() {
        return AgentHost::with(|state| {
            state.fail(
                Status::InvalidArgument,
                "pb_load_script: name/source 为空指针",
            )
        });
    }
    // SAFETY: 上面已判空；调用方保证字符串以 NUL 结尾且在调用期间存活
    let script_name = unsafe { cstr_to_string(name) };
    let script_source = unsafe { cstr_to_string(source) };
    let (Some(script_name), Some(script_source)) = (script_name, script_source) else {
        return AgentHost::with(|state| {
            state.fail(
                Status::InvalidArgument,
                "pb_load_script: 参数不是有效的 UTF-8",
            )
        });
    };
    mapped(
        AgentHost::call(|state| {
            state.require(Capability::ScriptLoad)?;
            if script_name.trim().is_empty() || script_source.trim().is_empty() {
                return Err(Error::Script("脚本名或正文为空".into()));
            }
            Err(Error::Trace(
                "pb_load_script: 需要内嵌 CPython 运行时（M0 桩实现，见 docs/roadmap.md 的 M1）"
                    .into(),
            ))
        }),
        Status::NotSupported,
    )
}

/// 开始录制轨迹（M0 已可用）。
///
/// # Safety
///
/// `path` 必须是 NUL 结尾的有效 UTF-8 字符串（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_start_trace(path: *const c_char, fold: c_uint) -> c_int {
    if path.is_null() {
        return AgentHost::with(|state| {
            state.fail(Status::InvalidArgument, "pb_start_trace: path 为空指针")
        });
    }
    // SAFETY: 上面已判空；调用方保证字符串以 NUL 结尾
    let Some(path) = (unsafe { cstr_to_string(path) }) else {
        return AgentHost::with(|state| {
            state.fail(
                Status::InvalidArgument,
                "pb_start_trace: path 不是有效的 UTF-8",
            )
        });
    };
    let meta = format!(
        "{{\"tool\":\"pinbridge-agent\",\"abel_version\":{PB_ABI_VERSION},\"fold\":{}}}",
        fold != 0
    );
    AgentHost::call(|state| {
        state.require(Capability::TraceRecord)?;
        state.start_trace(std::path::Path::new(&path), &meta, fold != 0)
    })
}

/// 停止录制并把汇总写回两个出参。
///
/// # Safety
///
/// `out_records` / `out_events` 必须是指向 `u64` 的可写指针（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_stop_trace(out_records: *mut u64, out_events: *mut u64) -> c_int {
    let (code, summary) = AgentHost::with(|state| {
        if !state.capabilities().contains(Capability::TraceRecord) {
            let code = state.fail(
                Status::CapabilityDenied,
                "pb_stop_trace: 缺少 trace_record 能力",
            );
            return (code, None);
        }
        match state.stop_trace() {
            Ok(summary) => (Status::Ok.code(), Some(summary)),
            Err(e) => (state.fail(Status::Internal, &e.to_string()), None),
        }
    });
    if let Some(summary) = summary {
        // SAFETY: 已判空；调用方保证这两个指针指向有效的 u64
        unsafe {
            if !out_records.is_null() {
                *out_records = summary.records;
            }
            if !out_events.is_null() {
                *out_events = summary.events;
            }
        }
    }
    code
}

/// 统一的桩实现：先校验能力，再返回"未实现"。
///
/// 顺序不能反（见文件头说明）：先校验能力才能让"权限链路"在 M0 阶段就被测试覆盖。
fn stub(capability: Capability, what: &str) -> c_int {
    mapped(
        AgentHost::call(|state| {
            state.require(capability)?;
            Err(Error::Trace(format!(
                "{what}: 需要 Pin 运行时（M0 桩实现）"
            )))
        }),
        Status::NotSupported,
    )
}

/// 把 `Internal` 重映射成"未实现"。
///
/// 为什么需要它：桩实现内部用的是 `Error::Trace`（因为它确实是个"能力之外"的
/// 执行问题），但对外必须报 `not_supported`——让调用方一眼看出"这条路径还没做"，
/// 而不是"出错了"。这两者的排查方向完全不同。
fn mapped(code: c_int, fallback: Status) -> c_int {
    if code == Status::Internal.code() {
        fallback.code()
    } else {
        code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinbridge_core::capability::CapabilitySet;
    use pinbridge_trace::event::{Event, EventKind};

    /// 全局状态在测试间共享，因此把有状态的断言集中在一个测试里顺序执行，
    /// 并用 `GLOBAL_STATE_LOCK` 与其它触碰全局状态的测试互斥。
    #[test]
    fn abi_surface_smoke() {
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(pb_abi_version(), PB_ABI_VERSION);

        // 1) 附加
        assert_eq!(pb_attach(4242), Status::Ok.code());
        assert_eq!(pb_is_attached(), 1);

        // 2) 默认无能力 → 内存读必须被能力拦下
        assert_eq!(pb_set_capabilities(0), Status::Ok.code());
        assert_eq!(pb_get_capabilities(), 0);
        let mut buffer = [0u8; 16];
        let mut written: c_uint = 0;
        let code = unsafe { pb_memory_read(0x1000, 4, buffer.as_mut_ptr(), 16, &mut written) };
        assert_eq!(code, Status::CapabilityDenied.code(), "无能力时必须被拒");

        // 3) 给足能力 → 桩实现返回 not_supported（而不是静默成功）
        assert_eq!(pb_set_capabilities(u32::MAX), Status::Ok.code());
        let code = unsafe { pb_memory_read(0x1000, 4, buffer.as_mut_ptr(), 16, &mut written) };
        assert_eq!(
            code,
            Status::NotSupported.code(),
            "M0 桩必须显式失败，不能假装成功"
        );

        // 4) 越界参数
        let code = unsafe { pb_memory_read(0x1000, 32, buffer.as_mut_ptr(), 16, &mut written) };
        assert_eq!(code, Status::InvalidArgument.code());

        // 5) 事件热路径：分配序号 → 推送 → 取回
        let sequence = pb_allocate_sequence();
        let event =
            Event::new(sequence, EventKind::BreakpointHit, 7, 0x1400_01000).with_arg(0, 0xAB);
        let raw = PbEventRaw::from_event(&event);
        assert_eq!(unsafe { pb_push_event(&raw) }, Status::Ok.code());

        let mut out = [PbEventRaw::EMPTY; 4];
        let mut count: c_uint = 0;
        let code = unsafe { pb_drain_events(out.as_mut_ptr(), 4, &mut count) };
        assert_eq!(code, Status::Ok.code());
        assert_eq!(count, 1);
        assert_eq!(out[0].to_event(), Some(event));

        // 6) 空指针/零容量的防御
        assert_eq!(
            unsafe { pb_push_event(std::ptr::null()) },
            Status::InvalidArgument.code()
        );
        assert_eq!(
            unsafe { pb_drain_events(std::ptr::null_mut(), 4, &mut count) },
            Status::InvalidArgument.code()
        );
        assert_eq!(
            unsafe { pb_drain_events(out.as_mut_ptr(), 0, &mut count) },
            Status::InvalidArgument.code()
        );

        // 7) 错误文本可读且非空
        let message = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
        assert!(!message.is_empty(), "失败之后 pb_last_error 必须有内容");

        // 8) 轨迹录制（M0 已可用）
        let dir = std::env::temp_dir().join(format!("pb-agent-abi-{}", std::process::id()));
        let path = dir.join("abi.pbt");
        let path_c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(
            unsafe { pb_start_trace(path_c.as_ptr(), 1) },
            Status::Ok.code()
        );
        for i in 1..=120u64 {
            let ev = Event::new(i, EventKind::SyscallEnter, 5, 0x2000).with_arg(0, 0x40);
            let raw = PbEventRaw::from_event(&ev);
            assert_eq!(unsafe { pb_push_event(&raw) }, Status::Ok.code());
        }
        let mut records = 0u64;
        let mut events = 0u64;
        assert_eq!(
            unsafe { pb_stop_trace(&mut records, &mut events) },
            Status::Ok.code()
        );
        assert_eq!(events, 120);
        assert_eq!(records, 2, "120 条相同事件应折叠成『代表 + 标记』");
        assert!(path.exists());

        // 9) 能力集读回
        assert!(CapabilitySet::from_bits(pb_get_capabilities()).contains(Capability::ScriptLoad));

        // 10) detach
        assert_eq!(pb_detach(), Status::Ok.code());
        assert_eq!(pb_is_attached(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn kind_name_never_returns_null() {
        for code in 0..15u32 {
            let ptr = pb_kind_name(code);
            assert!(!ptr.is_null());
            let name = unsafe { cstr_to_string(ptr) }.unwrap();
            assert!(!name.is_empty());
        }
        assert_eq!(
            unsafe { cstr_to_string(pb_kind_name(1)) }.unwrap(),
            "breakpoint_hit"
        );
        assert_eq!(
            unsafe { cstr_to_string(pb_kind_name(999)) }.unwrap(),
            "unknown"
        );
    }
}
