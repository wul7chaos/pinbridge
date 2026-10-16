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
use pinbridge_sys::abi::{cstr_to_string, PbEventRaw, Status, PB_ABI_VERSION};

use crate::host::{kind_name, AgentHost};
use pinbridge_trace::event::EventKind;

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
    flush_breakpoint_hits();
    // ── 先分发，再入队 ──
    //
    // 顺序是刻意的：脚本可能在回调里 `pb.mark`/`pb.log`，那些输出会产生
    // **新的** ScriptOutput 事件。先分发再入队，能让这些输出紧跟在
    // "触发它的事件"之后出现在事件流里；反过来会让因果顺序看起来是反的。
    dispatch_to_scripts(&event);
    AgentHost::call(|state| state.push(&event).map(|_| ()))
}

/// 把一条入站事件交给脚本的订阅者，并把脚本产生的输出转成事件。
///
/// 这里是"探针 → 脚本 → 轨迹"这条链的中间一环。三个约束：
///
/// 1. **没有脚本时不碰解释器**：绝大多数运行里没有脚本，这条路径必须近乎零成本；
/// 2. 事件名用 `EventKind::name()`（`syscall_enter` 这种），与脚本里
///    `pb.subscribe("syscall_enter", ...)` 的写法一致——两处名字不一致的话，
///    脚本会"注册成功但永远不被调用"，那是最难查的一类问题；
/// 3. 脚本抛异常不能把探针打死：错误只记录，不让 `pb_push_event` 失败。
fn dispatch_to_scripts(event: &PbEventRaw) {
    let Some(kind) = EventKind::from_code(event.kind) else {
        return; // 未知类型：不是脚本要关心的东西
    };
    let has_script =
        AgentHost::with(|state| !state.script_host().subscribers(event.kind).is_empty());
    if !has_script {
        return;
    }
    let payload = serde_json::json!({
        "sequence": event.sequence,
        "thread": event.thread_id,
        "address": event.address,
        "args": event.args,
    })
    .to_string();
    let outcome = crate::python_host::dispatch(kind.name(), &payload);
    match outcome {
        Ok(called) => {
            if called != 0 {
                let (outputs, _) = collect_script_outputs();
                emit_script_outputs(&outputs);
            }
        }
        // 脚本出错**不能让探针失败**：记录到 last_error 里就够了。
        // 否则一个写错的脚本会把整条插装链路带崩，而那与"探针是否工作"无关。
        Err(error) => {
            AgentHost::with(|state| {
                // `Status` 里没有"脚本错误"这一档（它是 ABI 的固定集合），
                // 用 `Internal` 并在文本里说清影响面——调用方最需要知道的是
                // **探针没有受影响**，而不是错误被归到哪个码。
                state.fail(
                    Status::Internal,
                    &format!("脚本分发失败（探针不受影响）：{error}"),
                )
            });
        }
    }
}

/// 取走脚本输出，返回（输出条目，订阅的事件类型码）。
///
/// 队列项的编码是 `[kind, value, text]`：1 = log、2 = mark、3 = subscribe。
/// 订阅项在这里被翻译成**事件类型码**（分发侧按码查找）。
fn collect_script_outputs() -> (Vec<(u64, u64, String)>, Vec<u8>) {
    let mut outputs = Vec::new();
    let mut subscribed = Vec::new();
    let Ok(text) = crate::python_host::drain_outputs() else {
        return (outputs, subscribed);
    };
    let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
        return (outputs, subscribed);
    };
    for entry in entries {
        let Some(items) = entry.as_array() else {
            continue;
        };
        let kind = items
            .first()
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let value = items
            .get(1)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let text = items
            .get(2)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if kind == 3 {
            // 订阅：把名字翻译成类型码。翻不出来要说出来——"订阅了一个
            // 拼错的事件名"必须能被看见，否则脚本会静默地收不到事件。
            match event_kind_by_name(&text) {
                Some(code) => {
                    if !subscribed.contains(&code) {
                        subscribed.push(code);
                    }
                }
                None => {
                    AgentHost::with(|state| {
                        let _ = state.fail(
                            Status::InvalidArgument,
                            &format!("脚本订阅了未知事件名 \"{text}\"（见 EventKind 的名字表）"),
                        );
                    });
                }
            }
            continue;
        }
        outputs.push((kind, value, text));
    }
    (outputs, subscribed)
}

/// 事件名 → 类型码。名字表来自 `EventKind::name()`，与文档里的写法一致。
fn event_kind_by_name(name: &str) -> Option<u8> {
    // 覆盖全部类型：少一个名字就意味着那个事件"订阅了但收不到"。
    (0u8..=13)
        .filter_map(EventKind::from_code)
        .find(|kind| kind.name() == name)
        .map(EventKind::code)
}

/// 把 `log` / `mark` 输出转成 `ScriptOutput` 事件。
fn emit_script_outputs(outputs: &[(u64, u64, String)]) {
    for (kind, value, _text) in outputs {
        let mut raw = PbEventRaw::EMPTY;
        raw.kind = EventKind::ScriptOutput.code();
        raw.thread_id = 0;
        // 事件的 args 是**数字**（`.pbt` 的模型就是这样）：类别与数值在这里，
        // 文本目前只留在脚本侧的输出里（审计通道见 docs/design/script-runtime.md）。
        raw.address = *value;
        raw.args[0] = *kind;
        raw.args[1] = *value;
        // `push` 返回状态码（不是 Result）：输出推不进去（例如队列满）不该让
        // 调用方以为脚本执行失败；丢弃由事件队列自己的 dropped 计数记录。
        let _ = AgentHost::with(|state| state.push(&raw).is_ok());
    }
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
    // 先把异常上下文攒下的断点命中落成事件，再连同队列一起取走——
    // 否则"命中了但事件要等下一次推送才出现"会让调用方以为断点没生效。
    flush_breakpoint_hits();
    // SAFETY: 由调用方保证缓冲区大小与 capacity 一致
    let buffer = unsafe { std::slice::from_raw_parts_mut(out, capacity as usize) };
    let count = AgentHost::with(|state| state.drain_into(buffer));
    // SAFETY: written 已判空
    unsafe { *written = count as c_uint };
    Status::Ok.code()
}

/// 把异常上下文记下的断点命中转成事件。
///
/// **只在正常上下文调用**（`pb_drain_events` / `pb_push_event` 进来时）：
/// 向量化异常处理器里不适合分配内存、也不适合抢 agent 的全局状态锁
/// （如果异常正好发生在持锁期间，处理器会自己等自己）。
fn flush_breakpoint_hits() {
    for hit in crate::breakpoint::take_pending() {
        let mut raw = PbEventRaw::EMPTY;
        raw.sequence = pb_allocate_sequence();
        raw.kind = EventKind::BreakpointHit.code();
        raw.thread_id = hit.thread_id;
        raw.address = hit.address;
        // args[0] = 1：**命中后已自动解除**。调用方据此知道"想重复命中要再下"，
        // 而不是等一个永远不会来的第二次命中。
        raw.args[0] = 1;
        let _ = AgentHost::with(|state| state.push(&raw).is_ok());
    }
}

/// 取回一个统计量：`which` 0=丢弃数 1=积压 2=已取走。/// 取回一个统计量：`which` 0=丢弃数 1=积压 2=已取走。
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

/// 读目标内存。
///
/// ## 为什么用 `ReadProcessMemory`，而不是直接解引用
///
/// agent 就在目标进程里，`*(addr as *const u8)` 看着最直接，但它对**不可读地址**
/// 是把目标打崩（访问违例），而不是返回错误。被分析的程序里到处是未映射页、
/// 保护页、只在某个瞬间可读的区域——"读之前不知道能不能读"是常态。
///
/// `ReadProcessMemory(GetCurrentProcess(), …)` 会先做范围校验：
/// 读不了就返回 FALSE + 错误码，进程不受影响。代价是一次内核调用，
/// 换来的是一条**可失败的**读取路径。
///
/// # Safety
///
/// `out` 必须是指向 `out_capacity` 字节的可写缓冲区（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_memory_read(
    address: u64,
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
    if let Err(error) = AgentHost::with(|state| state.require(Capability::MemRead)) {
        return AgentHost::with(|state| state.fail(Status::CapabilityDenied, &error.to_string()));
    }
    // SAFETY: out 非空且容量 >= length（上面已校验）；out 指向 length 字节可写区。
    let buffer = unsafe { std::slice::from_raw_parts_mut(out, length as usize) };
    match crate::memory::read_process(address, buffer) {
        Ok(read) => {
            // SAFETY: out_written 已判空
            unsafe { *out_written = read as c_uint };
            Status::Ok.code()
        }
        Err(reason) => AgentHost::with(|state| {
            // 读失败的原因必须带**地址**：调用方（尤其是 AI）需要知道
            // 是"这个地址读不了"还是"权限/能力不够"，两者的下一步动作完全不同。
            state.fail(
                Status::Internal,
                &format!("pb_memory_read: 读 0x{address:x}（{length} 字节）失败——{reason}"),
            )
        }),
    }
}

/// 写目标内存（M0 桩）。
///
/// # Safety
///
/// `bytes` 必须指向 `length` 字节的可读缓冲区（或 `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_memory_write(address: u64, bytes: *const u8, length: c_uint) -> c_int {
    if bytes.is_null() && length != 0 {
        return AgentHost::with(|state| {
            state.fail(Status::InvalidArgument, "pb_memory_write: bytes 为空指针")
        });
    }
    if let Err(error) = AgentHost::with(|state| state.require(Capability::MemWrite)) {
        return AgentHost::with(|state| state.fail(Status::CapabilityDenied, &error.to_string()));
    }
    // SAFETY: bytes 非空（length==0 时读 0 字节，指针不参与运算）
    let source = unsafe { std::slice::from_raw_parts(bytes, length as usize) };
    match crate::memory::write_process(address, source) {
        Ok(written) if written == length as usize => Status::Ok.code(),
        // 部分写入必须**当作失败**报出来：调用方以为整段都写进去了，
        // 而实际只写了前半截——这类错误在后续某个时刻才显形，最难查。
        Ok(written) => AgentHost::with(|state| {
            state.fail(
                Status::Internal,
                &format!(
                    "pb_memory_write: 只写入了 {written}/{length} 字节（部分写入按失败处理）",
                ),
            )
        }),
        Err(reason) => AgentHost::with(|state| {
            state.fail(
                Status::Internal,
                &format!("pb_memory_write: 写 0x{address:x}（{length} 字节）失败——{reason}"),
            )
        }),
    }
}

/// 在 `address` 上下一个软件断点（`0xCC`）。
///
/// ## 语义：命中一次后**自动解除**
///
/// 传统做法是"命中 → 恢复原字节 → 单步 → 重装"，那段舞蹈在多线程目标里
/// 有真实竞态（恢复与重装之间别的线程可能正好执行到这条指令）。我们没有
/// 把目标线程停下来的能力，所以选了一条**明确的语义**而不是一个偶发的崩溃：
/// 命中即解除，事件里带 `args[0]=1` 标记；要重复命中就再下一次。
///
/// 这是 ABI 的既有导出，行为从"桩"变成"实现"，不需要提升 `PB_ABI_VERSION`。
#[unsafe(no_mangle)]
pub extern "C" fn pb_set_breakpoint(address: u64) -> c_int {
    if let Err(error) = AgentHost::with(|state| state.require(Capability::Breakpoint)) {
        return AgentHost::with(|state| state.fail(Status::CapabilityDenied, &error.to_string()));
    }
    match crate::breakpoint::arm(address) {
        Ok(()) => AgentHost::with(|state| {
            state.fail(
                Status::Ok,
                &format!(
                    "pb_set_breakpoint: 已在 0x{address:x} 下断点（命中一次后自动解除）；\
                     当前共 {} 个",
                    crate::breakpoint::armed_count()
                ),
            )
        }),
        Err(error) => AgentHost::with(|state| state.fail(Status::Internal, &error.to_string())),
    }
}

/// 清断点（M0 桩）。
///
/// 与 `pb_set_breakpoint` 成对存在：只给"下"不给"清"的接口会让使用者
/// （尤其是 AI）在换策略时无法收回已经下过的断点，只能重启会话。
/// 这是 ABI 的**追加**导出，老的加载器不解析它，因此不需要提升 `PB_ABI_VERSION`。
#[unsafe(no_mangle)]
pub extern "C" fn pb_clear_breakpoint(address: u64) -> c_int {
    if let Err(error) = AgentHost::with(|state| state.require(Capability::Breakpoint)) {
        return AgentHost::with(|state| state.fail(Status::CapabilityDenied, &error.to_string()));
    }
    match crate::breakpoint::disarm(address) {
        Ok(()) => AgentHost::with(|state| {
            state.fail(
                Status::Ok,
                &format!(
                    "pb_clear_breakpoint: 已解除 0x{address:x} 上的断点（原字节已写回）；\
                     还剩 {} 个",
                    crate::breakpoint::armed_count()
                ),
            )
        }),
        // 解一个不存在的断点**必须报错**：静默成功会让调用方一直以为那边有断点。
        Err(error) => {
            AgentHost::with(|state| state.fail(Status::InvalidArgument, &error.to_string()))
        }
    }
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
    // 空名/空正文是**调用方的 bug**，不是"权限"也不是"未实现"：
    // 先按 invalid_argument 报出来，别让它退化成一句含糊的"不支持"。
    if script_name.trim().is_empty() || script_source.trim().is_empty() {
        return AgentHost::with(|state| {
            state.fail(Status::InvalidArgument, "pb_load_script: 脚本名或正文为空")
        });
    }
    // 能力与策略检查都在**碰解释器之前**：一个没有 script_load 的调用方
    // 不该触发"加载 30 MB 的解释器"这种副作用，更不该因此拿到"Python 没装"
    // 这种与它无关的错误。
    // 复用 stub() 来生成"能力不足"的文本，避免两处说法不一致。
    if AgentHost::with(|state| state.require(Capability::ScriptLoad).is_err()) {
        return stub(Capability::ScriptLoad, "pb_load_script");
    }
    // 策略检查（静态文本）：它不需要解释器，因此能在没有 Python 的机器上
    // 先生效——这是刻意的，"注入前拦截"不该依赖运行时的存在。
    if let Err(violation) = crate::script::ScriptPolicy::check(&script_source) {
        return AgentHost::with(|state| {
            state.fail(
                Status::InvalidArgument,
                &format!(
                    "pb_load_script: 策略拒绝第 {} 行（{}）：{}",
                    violation.line, violation.token, violation.reason
                ),
            )
        });
    }
    // 到这里才碰解释器。三种结局，都**明确**说清：
    //   1. 没有解释器 → not_supported + "装 Python 或指定 PINBRIDGE_PYTHON_DLL"
    //   2. 引导失败   → internal + 原因（解释器在，但 `pb` 模块装不上）
    //   3. 脚本出错   → internal + CPython 已经打印的堆栈（`PyErr_Print`）
    if let Err(error) = crate::python_host::bootstrap() {
        return AgentHost::with(|state| {
            state.fail(
                Status::NotSupported,
                &format!("pb_load_script: 策略检查已通过，但解释器不可用——{error}"),
            )
        });
    }
    // 跑脚本：这一步之后脚本里的 `pb.subscribe(...)` 才真正注册上。
    let described = crate::python_host::library_description().unwrap_or_default();
    let run = {
        // 用 `dispatch` 之外的一条窄路径：这里需要"执行这段源码"，
        // 所以走宿主自己的 run（同样持 GIL）。
        crate::python_host::run_source(&script_source)
    };
    if let Err(error) = run {
        return AgentHost::with(|state| {
            state.fail(
                Status::Internal,
                &format!("pb_load_script: 脚本执行失败——{error}"),
            )
        });
    }
    // 登记脚本（名称 + 源码哈希 + 状态），并把它订阅的事件类型记下来。
    // 取回加载期输出，并从里面读出**脚本实际订阅了哪些事件**。
    //
    // 为什么以"脚本实际调用"为准而不是解析源码文本：`pb.subscribe` 可能写在
    // 条件分支里、可能由变量拼出名字，文本解析会给出一个"看起来对但没生效"的
    // 订阅列表——而分发侧只看这个列表，于是脚本会静默地永远收不到事件。
    let (outputs, subscribed) = collect_script_outputs();
    let registered = AgentHost::with(|state| {
        match state
            .script_host_mut()
            .register(&script_name, &script_source, subscribed)
        {
            Ok(script) => script.id,
            Err(error) => {
                let _ = state.fail(Status::Internal, &error.to_string());
                0
            }
        }
    });
    if registered != 0 {
        // 状态机迁移（Registered → Loaded）单独一步：登记成功但迁移失败也要能
        // 看见，而不是让"已登记但永远不会收到事件"变成沉默的事实。
        let _ = AgentHost::with(|state| state.script_host_mut().mark_loaded(registered).is_ok());
    }
    if registered == 0 {
        return AgentHost::with(|state| {
            state.fail(
                Status::Internal,
                "pb_load_script: 登记脚本失败（见 last_error）",
            )
        });
    }
    // 加载期的 log / mark 也要变成事件（订阅那几条已经用掉了）。
    emit_script_outputs(&outputs);
    AgentHost::with(|state| {
        state.fail(
            Status::Ok,
            &format!(
                "pb_load_script: 脚本 {registered} 已加载并执行（解释器 {described}）；\
                 订阅的回调会在对应事件到达时被调用。加载期输出已转成 script_output 事件。"
            ),
        )
    })
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
    // 元数据键名是 `abi_version`：它是**会被上游解析**的字段（Hub、Python 侧、
    // 事后审计都会读），写成 `abel_version` 这种错别字不会让任何东西立刻报错，
    // 只会让读取方永远拿不到 ABI 版本。这个 bug 是 Pin tool 第一次跑通、
    // 用 `pinbridge replay` 打印元数据时才看见的——"能跑通"不等于"内容对"。
    let meta = format!(
        "{{\"tool\":\"pinbridge-agent\",\"abi_version\":{PB_ABI_VERSION},\"fold\":{}}}",
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
    AgentHost::with(|state| match state.require(capability) {
        Ok(()) => state.fail(
            Status::NotSupported,
            &format!("{what}: 需要 Pin 运行时的支撑（M0 桩实现，见 docs/roadmap.md 的 M1）"),
        ),
        // 能力不足的文本由 Error 自己给（"能力不足: 需要 X，当前持有 Y"），
        // 这里只补上状态码，避免出现"码说不支持、文本说能力不足"的错配。
        Err(error) => state.fail(Status::CapabilityDenied, &error.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinbridge_core::capability::CapabilitySet;
    use pinbridge_trace::event::{Event, EventKind};

    /// 全局状态在测试间共享，因此把有状态的断言集中在一个测试里顺序执行，
    /// 并用 `GLOBAL_STATE_LOCK` 与其它触碰全局状态的测试互斥。
    /// 轨迹元数据里的键名是上游会解析的契约，别写错。
    #[test]
    fn trace_metadata_uses_the_documented_keys() {
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pb-meta-{}", std::process::id()));
        let path = dir.join("meta.pbt");
        let text = path.to_string_lossy().into_owned();
        // 只用导出面（而不是直接摸全局状态）：这样测的是调用方真正会走的那条路。
        pb_set_capabilities(Capability::operator_default().bits());
        // `pb_attach` 是普通 extern "C" fn（不是 unsafe fn），无需 unsafe 块
        assert_eq!(pb_attach(1), Status::Ok.code());
        let c_path = std::ffi::CString::new(text).unwrap();
        // SAFETY: 传入的是有效的 NUL 结尾字符串
        assert_eq!(
            unsafe { pb_start_trace(c_path.as_ptr(), 1) },
            Status::Ok.code()
        );
        // SAFETY: 空指针会让导出立刻返回错误，这里传 null 只为触发写入路径
        let mut records = 0u64;
        let mut events = 0u64;
        // SAFETY: 两个输出指针都指向本栈变量
        assert_eq!(
            unsafe { pb_stop_trace(&mut records, &mut events) },
            Status::Ok.code()
        );
        let bytes = std::fs::read(&path).expect("轨迹文件应当存在");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("abi_version"), "元数据键名必须是 abi_version");
        assert!(!text.contains("abel_version"), "错别字不该回来");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 端到端（导出面）：加载一个订阅 `syscall_enter` 的脚本 → 推送两条事件 →
    /// 脚本把看到的 SSN 用 `pb.mark` 报回来 → 输出变成 `script_output` 事件。
    ///
    /// 这条是 M1 的核心能力证据：**AI 写的脚本在目标进程里被探针事件驱动**。
    #[test]
    fn script_is_driven_by_pushed_events() {
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(pb_attach(1), Status::Ok.code());
        assert_eq!(
            pb_set_capabilities(Capability::ScriptLoad.bit() | Capability::MemRead.bit()),
            Status::Ok.code()
        );
        let name = std::ffi::CString::new("driver").unwrap();
        let source = std::ffi::CString::new(
            r#"
import pb

def on_syscall(ev):
    pb.mark("seen_ssn", ev.args[0])

pb.subscribe("syscall_enter", on_syscall)
"#,
        )
        .unwrap();
        // SAFETY: 两个 CString 在本调用期间存活
        let loaded = unsafe { pb_load_script(name.as_ptr(), source.as_ptr()) };
        if loaded == Status::NotSupported.code() {
            // 没装 Python 的环境（例如 CI 的 ubuntu 任务）：这条用例跳过，
            // 但**不是静默通过**——原因必须能从 last_error 读出来。
            // SAFETY: 立刻拷贝成 String
            let text = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
            assert!(text.contains("解释器"), "跳过时必须说清原因：{text}");
            println!("跳过：{text}");
            return;
        }
        assert_eq!(loaded, Status::Ok.code(), "加载脚本应当成功");

        // 推一条系统调用进入事件（kind=3，args[0]=SSN=0x50）
        let mut raw = PbEventRaw::EMPTY;
        raw.kind = EventKind::SyscallEnter.code();
        raw.sequence = 1;
        raw.address = 0x7ff6_0000_2000;
        raw.args[0] = 0x50;
        // SAFETY: raw 指向本栈变量
        assert_eq!(unsafe { pb_push_event(&raw) }, Status::Ok.code());

        // 循环取空：事件队列是**全进程共享**的，别的用例也往里面推过东西。
        // 只取一批（容量 16）会把本次的输出挤到批次之外——那是在测"别的用例
        // 推了多少事件"，不是在测这条链路。上限只是防止取空失败时死循环。
        let mut all: Vec<PbEventRaw> = Vec::new();
        for _ in 0..64 {
            let mut buffer = [PbEventRaw::EMPTY; 64];
            let mut written = 0u32;
            // SAFETY: 缓冲区与计数都指向本栈变量
            assert_eq!(
                unsafe { pb_drain_events(buffer.as_mut_ptr(), 64, &mut written) },
                Status::Ok.code()
            );
            if written == 0 {
                break;
            }
            all.extend_from_slice(&buffer[..written as usize]);
        }
        let ours: Vec<&PbEventRaw> = all
            .iter()
            .filter(|e| {
                e.kind == EventKind::ScriptOutput.code() && e.args[0] == 2 && e.args[1] == 0x50
            })
            .collect();
        assert_eq!(
            ours.len(),
            1,
            "脚本应当恰好把看到的那条 SSN 报回来一次（共取到 {} 条事件）",
            all.len()
        );
        assert_eq!(ours[0].args[1], 0x50, "报回来的值必须就是事件里的 args[0]");
        println!("✓ 脚本被事件驱动，输出进了事件流：{:?}", ours[0]);
    }
    /// 下断点时地址打错**必须是错误**，不能把目标打崩。
    ///
    /// 这条不是理论问题：这个缺陷真实发生过一次——把 `SetBreakpoint` 接到
    /// 真实实现之后，一条拿 `0x1000` 当"假地址"的老用例让整个测试进程
    /// `STATUS_ACCESS_VIOLATION`。
    /// Windows 专有：靶函数与软件断点都只在 Windows 上存在。
    #[cfg(windows)]
    #[test]
    fn breakpoint_on_an_unmapped_address_is_an_error_not_a_crash() {
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(pb_attach(1), Status::Ok.code());
        assert_eq!(
            pb_set_capabilities(Capability::Breakpoint.bit()),
            Status::Ok.code()
        );
        let code = pb_set_breakpoint(0x1000);
        assert_eq!(code, Status::Internal.code(), "应当是错误，不是崩溃");
        // SAFETY: 立刻拷贝
        let text = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
        assert!(text.contains("0x1000"), "错误要带上地址：{text}");
        assert!(
            text.contains("读不到") || text.contains("未映射"),
            "错误要说清是地址问题：{text}"
        );
        assert_eq!(crate::breakpoint::armed_count(), 0, "失败的断点不该进表");
        println!("✓ 不可映射地址下断点报错而不是崩溃：{text}");
    }
    /// 导出面：断点的能力闸门、命中事件、解除。
    /// Windows 专有：靶函数与软件断点都只在 Windows 上存在。
    #[cfg(windows)]
    #[test]
    fn breakpoints_fire_and_report_through_the_exports() {
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(pb_attach(1), Status::Ok.code());
        crate::breakpoint::disarm_all();
        let address = crate::breakpoint::tests::pinbridge_breakpoint_target as *const () as u64;

        // 1) 没有 breakpoint 能力：必须被拒，且**真的没下上去**
        assert_eq!(pb_set_capabilities(0), Status::Ok.code());
        assert_eq!(
            pb_set_breakpoint(address),
            Status::CapabilityDenied.code(),
            "没能力时必须拒绝"
        );
        assert_eq!(
            crate::breakpoint::armed_count(),
            0,
            "被拒的断点绝不能真的下上去"
        );

        // 2) 给能力 → 下断点 → 触发它
        assert_eq!(
            pb_set_capabilities(Capability::Breakpoint.bit() | Capability::MemRead.bit()),
            Status::Ok.code()
        );
        assert_eq!(
            pb_set_breakpoint(address),
            Status::Ok.code(),
            "应当下断点成功"
        );
        assert_eq!(crate::breakpoint::armed_count(), 1);
        // 调用靶函数：命中由异常处理器记录
        let value = crate::breakpoint::tests::pinbridge_breakpoint_target(5);
        assert_eq!(value, 16, "命中之后函数仍然要正常跑完：{value}");

        // 3) 事件必须出现（命中 → 事件由正常上下文落成）
        let mut all: Vec<PbEventRaw> = Vec::new();
        for _ in 0..8 {
            let mut buffer = [PbEventRaw::EMPTY; 32];
            let mut written = 0u32;
            // SAFETY: 缓冲区与计数指向本栈变量
            assert_eq!(
                unsafe { pb_drain_events(buffer.as_mut_ptr(), 32, &mut written) },
                Status::Ok.code()
            );
            if written == 0 {
                break;
            }
            all.extend_from_slice(&buffer[..written as usize]);
        }
        let hits: Vec<&PbEventRaw> = all
            .iter()
            .filter(|e| e.kind == EventKind::BreakpointHit.code() && e.address == address)
            .collect();
        assert_eq!(hits.len(), 1, "应当恰好有一条断点命中事件：{all:?}");
        assert_eq!(hits[0].args[0], 1, "args[0]=1 表示命中后已自动解除");
        println!("✓ 断点命中事件：{:?}", hits[0]);

        // 4) 命中即解除 → 再解一次必须报错（而不是静默成功）
        assert_eq!(crate::breakpoint::armed_count(), 0, "命中后应当自动解除");
        assert_eq!(
            pb_clear_breakpoint(address),
            Status::InvalidArgument.code(),
            "解一个已经解除的断点必须报错"
        );

        // 5) 正常解除路径
        assert_eq!(pb_set_breakpoint(address), Status::Ok.code());
        assert_eq!(pb_clear_breakpoint(address), Status::Ok.code());
        assert_eq!(crate::breakpoint::armed_count(), 0);
        // SAFETY: 读回那个字节
        let byte = unsafe { std::ptr::read_volatile(address as *const u8) };
        assert_ne!(byte, 0xCC, "解除之后不能留下 int3");
    }
    /// 导出面：真实的内存读写（Hub 侧走的就是这几个导出）。
    ///
    /// 用 agent 自己的静态缓冲区当靶子：地址确定、不需要额外进程。
    #[test]
    fn memory_read_and_write_go_through_the_real_path() {
        static mut BUFFER: [u8; 32] = [0u8; 32];
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(pb_attach(1), Status::Ok.code());
        let address = std::ptr::addr_of_mut!(BUFFER).cast::<u8>() as u64;

        // 1) 没有能力：写必须被拦下，**且不能真的写进去**
        assert_eq!(pb_set_capabilities(0), Status::Ok.code());
        let pattern = b"PBRW-EXPORT";
        // SAFETY: pattern 有 pattern.len() 字节
        let denied = unsafe { pb_memory_write(address, pattern.as_ptr(), 11) };
        assert_eq!(denied, Status::CapabilityDenied.code(), "没能力时必须拒绝");
        // SAFETY: 地址指向本测试的静态缓冲区
        let untouched = unsafe { *std::ptr::addr_of!(BUFFER) };
        assert_eq!(untouched, [0u8; 32], "被拒绝的写绝不能真的落下去");

        // 2) 给读写能力：写进去，再读回来
        //
        // 读写往返是 **Windows** 的行为（M1 只在 Windows 上实现进程内读写）。
        // 别的平台上这里必须明确报"平台未实现"，而**不能**假装成功——
        // 所以两个平台各有自己的断言，而不是把 Windows 的行为当通用契约。
        let bits = Capability::MemRead.bit() | Capability::MemWrite.bit();
        assert_eq!(pb_set_capabilities(bits), Status::Ok.code());
        // SAFETY: 同上
        let write_code = unsafe { pb_memory_write(address, pattern.as_ptr(), 11) };
        if !cfg!(windows) {
            assert_eq!(
                write_code,
                Status::Internal.code(),
                "非 Windows 平台必须明确报未实现"
            );
            // SAFETY: 立刻拷贝
            let text = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
            assert!(
                text.contains("未实现") || text.contains("Windows"),
                "原因要说清是平台限制：{text}"
            );
            println!("✓ 非 Windows 平台明确报未实现：{text}");
            return;
        }
        assert_eq!(write_code, Status::Ok.code(), "有 mem_write 时写应当成功");
        let mut buffer = [0u8; 11];
        let mut written = 0u32;
        // SAFETY: buffer 有 11 字节，written 指向本栈变量
        assert_eq!(
            unsafe { pb_memory_read(address, 11, buffer.as_mut_ptr(), 11, &mut written) },
            Status::Ok.code()
        );
        assert_eq!(written, 11);
        assert_eq!(&buffer, pattern, "读回来的必须就是写进去的");
        println!("✓ 导出面读写可用：{:?}", String::from_utf8_lossy(&buffer));

        // 3) 不可读地址：必须是**错误**而不是崩溃（这条路径以前是直接解引用）
        let mut sink = [0u8; 8];
        // SAFETY: sink 有 8 字节
        let code =
            unsafe { pb_memory_read(0xDEAD_BEEF_0000, 8, sink.as_mut_ptr(), 8, &raw mut written) };
        assert_eq!(code, Status::Internal.code(), "不可读地址应当报错");
        // SAFETY: 立刻拷贝
        let text = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
        assert!(
            text.contains("0xdeadbeef0000"),
            "错误文本要带上地址：{text}"
        );
        println!("✓ 不可读地址报错且带地址：{text}");
    }
    /// 策略检查必须**先于**解释器：这条顺序让"注入前拦截"在没有 Python 的
    /// 机器上也成立，而且违规脚本的报错不会掺进"Python 没装"这种无关信息。
    #[test]
    fn script_policy_runs_before_the_interpreter() {
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(pb_attach(1), Status::Ok.code());
        // 只给脚本能力，不给别的：这样"被拦下"的原因只可能是策略或解释器。
        assert_eq!(
            pb_set_capabilities(Capability::ScriptLoad.bit()),
            Status::Ok.code()
        );
        let name = std::ffi::CString::new("demo").unwrap();
        let violating = std::ffi::CString::new("import os\nos.system('calc')\n").unwrap();
        // SAFETY: 两个 CString 在本调用期间存活
        let code = unsafe { pb_load_script(name.as_ptr(), violating.as_ptr()) };
        assert_eq!(
            code,
            Status::InvalidArgument.code(),
            "策略违规必须是 invalid_argument（不是 not_supported）"
        );
        // SAFETY: pb_last_error 返回 DLL 内的 NUL 结尾字符串，立刻拷贝成本地 String
        let text = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
        assert!(text.contains("策略拒绝"), "错误必须点名是策略拒绝：{text}");
        assert!(
            !text.contains("解释器"),
            "策略拒绝的文本里不该出现解释器（那说明顺序反了）：{text}"
        );
        println!("✓ 策略先于解释器：{text}");

        // 干净的脚本：这里才轮到解释器。两种平台差异都由这一层表达：
        //   * 有解释器 → 真的加载成功（`Ok`），并说清脚本已登记；
        //   * 没有解释器（例如 CI 的 ubuntu 任务）→ `not_supported`，
        //     原因必须指向解释器，而不是含糊的"不支持"。
        let clean = std::ffi::CString::new("value = 1 + 1\nassert value == 2\n").unwrap();
        // SAFETY: 同上
        let code = unsafe { pb_load_script(name.as_ptr(), clean.as_ptr()) };
        // SAFETY: 同上
        let text = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
        if code == Status::Ok.code() {
            assert!(
                text.contains("解释器"),
                "成功路径也要说清用的是哪个解释器：{text}"
            );
            println!("✓ 干净脚本已加载：{text}");
        } else {
            assert_eq!(code, Status::NotSupported.code(), "实际: {code}");
            assert!(text.contains("解释器"), "原因必须指向解释器这一层：{text}");
            println!("✓ 无解释器环境（明确报因）：{text}");
        }
    }
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

        // 3) 给足能力 → 真实读取。这里故意用一个几乎不可能被映射的地址：
        //    要求是"**失败并说清原因**"，而不是静默成功。
        //    （真实可读地址的往返在 `memory_read_and_write_go_through_the_real_path` 里测。）
        assert_eq!(pb_set_capabilities(u32::MAX), Status::Ok.code());
        let code = unsafe { pb_memory_read(0x1000, 4, buffer.as_mut_ptr(), 16, &mut written) };
        assert_eq!(
            code,
            Status::Internal.code(),
            "不可读地址必须显式失败，不能假装成功"
        );
        // SAFETY: 立刻拷贝成 String
        let text = unsafe { cstr_to_string(pb_last_error()) }.unwrap_or_default();
        assert!(text.contains("0x1000"), "失败原因要带上地址：{text}");
        // 系统错误码只有 Windows 有；别的平台上这条契约是"说清平台未实现"。
        // 把某个平台的行为当成通用契约写过一次，CI 上就会红一次。
        if cfg!(windows) {
            assert!(text.contains("Windows 错误"), "要带上系统错误码：{text}");
        } else {
            assert!(
                text.contains("未实现") || text.contains("Windows"),
                "非 Windows 平台要说清是平台限制：{text}"
            );
        }

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
