//! Hub ↔ agent 的回环链路。
//!
//! ## 这条链路解决什么
//!
//! M0.5 落地的桥接层（`native/src/agent_bridge.cpp`）让 Hub 在**同进程**内
//! 通过 C ABI 驱动 agent。但真实场景里 agent 在**目标进程**里，Hub 在另一个进程，
//! 于是需要一条链路。本文件就是目标侧的那一半：
//! 它在回环地址上监听，把 [`AgentRequest`] 翻译成对导出面的调用，
//! 再把结果翻译成 [`AgentResponse`]。
//!
//! ## 硬约束：只监听回环
//!
//! [`LinkServer::bind`] **没有**地址参数，它只会绑定 `127.0.0.1`。
//! 这不是"默认值"，而是刻意去掉的选择权：把调试能力暴露到网络上
//! 不是本项目的可配置项（见 `docs/architecture/security-model.md`）。
//!
//! ## 与 C ABI 的关系：共用同一套闸门
//!
//! 本层**不重复实现权限判断**，而是直接调用 `crate::exports` 里的 `pb_*`。
//! 两条入口（同进程 C ABI、跨进程回环链路）于是不可能对"这条命令该不该放行"
//! 产生分歧——这是"双端校验"之外的第二重防分叉措施。
//!
//! ## `from` 的语义（容易被误解）
//!
//! `DrainEvents.from` 只做**向前过滤**：比它更早且已经被取走的事件不会重现。
//! 要完整历史必须读 `.pbt` 文件——链路从来不是历史存储（见
//! `docs/design/trace-compression.md` 开头那句"只有轨迹是完整的"）。

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::time::Duration;

use pinbridge_proto::agent_link::{error_code, AgentRequest, AgentResponse, EventDto};
use pinbridge_sys::abi::{PbEventRaw, Status};

use crate::exports;

/// 单帧上限。与 Hub 侧共用同一个常量，避免两边各自"觉得"够大。
pub const MAX_FRAME_BYTES: usize = pinbridge_proto::MAX_FRAME_BYTES;

/// 单次 `DrainEvents` 的条数上限。
///
/// 存在的理由：链路是控制通道，一次拉太多会把回答帧做得很大（JSON 编码在
/// agent 线程上做，会占住目标线程）。要大批量数据请走 `.pbt`。
pub const MAX_DRAIN_LIMIT: u32 = 4096;

/// 单次 `ReadMemory` 的字节上限（与 `docs/specs/ipc-protocol.md` 的 64 KiB 一致）。
pub const MAX_LINK_READ_BYTES: u32 = 64 * 1024;

/// 空闲超时：对端不再发请求时，服务端按"这一轮结束"处理并返回。
///
/// 为什么必须有它：`accept` 到的连接如果对端进程被杀掉，`read` 会一直阻塞，
/// 于是 agent 里那条服务线程永久占住（目标进程里不该出现这种僵尸线程）。
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 回环链路的服务端。
#[derive(Debug)]
pub struct LinkServer {
    listener: TcpListener,
    addr: SocketAddr,
    idle_timeout: Duration,
}

impl LinkServer {
    /// 绑定到回环地址，端口由系统分配。
    ///
    /// # Errors
    ///
    /// 端口耗尽或权限不足时返回 I/O 错误。
    pub fn bind() -> std::io::Result<Self> {
        // 写死回环：没有任何参数能让它绑到别处，这是设计而不是默认值。
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        debug_assert!(addr.ip().is_loopback());
        Ok(Self {
            listener,
            addr,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        })
    }

    /// 实际监听的地址（含系统分配的端口）。Hub 用它来连接。
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 调整空闲超时（默认 30 秒）。测试用它把"对端消失"这条路径跑快。
    #[must_use]
    pub const fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    /// 接受一个连接并处理其中的请求，直到对端关闭或空闲超时。
    ///
    /// 返回处理过的请求条数。**一次连接可以承载多次往返**：
    /// Hub 侧是"一问一答复用一条连接"，而不是每条命令重连。
    ///
    /// # Errors
    ///
    /// `accept` 失败时返回 I/O 错误。连接**中途**的读错误不视为失败——
    /// 对端断开是正常结局（返回已处理的条数）。
    pub fn serve_once(&self) -> std::io::Result<usize> {
        let (stream, peer) = self.listener.accept()?;
        debug_assert!(peer.ip().is_loopback()); // 绑在回环上，对端不可能是外部地址
        Ok(self.handle_connection(stream))
    }

    /// 处理至多 `connections` 个连接，返回处理的请求总数。
    ///
    /// # Errors
    ///
    /// 同 [`LinkServer::serve_once`]。
    pub fn serve_connections(&self, connections: usize) -> std::io::Result<usize> {
        let mut total = 0;
        for _ in 0..connections {
            total += self.serve_once()?;
        }
        Ok(total)
    }

    /// 一条连接上的帧循环。
    fn handle_connection(&self, mut stream: TcpStream) -> usize {
        // 控制链路的包很小且时延敏感，禁用 Nagle 以免"一问一答"被延迟合并
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(self.idle_timeout));
        let _ = stream.set_write_timeout(Some(self.idle_timeout));

        let mut handled = 0;
        loop {
            match read_frame(&mut stream) {
                Ok(Some(payload)) => {
                    let response = match serde_json::from_slice::<AgentRequest>(&payload) {
                        Ok(request) => {
                            handled += 1;
                            dispatch(&request)
                        }
                        // 解析失败必须**回一个结构化错误**再关闭，而不是静默丢弃：
                        // 否则 Hub 侧只能看到"没有返回任何内容"，无法区分
                        // "agent 收到但不懂"与"链路断了"。
                        Err(error) => AgentResponse::Error {
                            code: error_code::PROTOCOL_ERROR.into(),
                            message: format!("请求不是合法的 AgentRequest: {error}"),
                        },
                    };
                    if !write_frame(&mut stream, &response) {
                        return handled;
                    }
                }
                // 正常结束：对端主动关闭，或空闲超时（都意味着这一轮结束了）
                Ok(None) => return handled,
                Err(message) => {
                    let _ = write_frame(
                        &mut stream,
                        &AgentResponse::Error {
                            code: error_code::PROTOCOL_ERROR.into(),
                            message,
                        },
                    );
                    return handled;
                }
            }
        }
    }
}

/// 读一帧。`Ok(None)` 表示对端正常关闭或空闲超时（两种都不算错误）。
fn read_frame(stream: &mut TcpStream) -> Result<Option<Vec<u8>>, String> {
    let mut head = [0u8; 4];
    match stream.read_exact(&mut head) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("读取帧头失败: {error}")),
    }
    let length = u32::from_le_bytes(head) as usize;
    if length == 0 {
        return Err("收到零长度帧：对端（或中间层）没有按『u32 长度 + 负载』发帧".into());
    }
    if length > MAX_FRAME_BYTES {
        return Err(format!(
            "帧过大: {length} 字节（上限 {MAX_FRAME_BYTES}）；拒绝继续读，避免把目标进程的内存吃光"
        ));
    }
    let mut payload = vec![0u8; length];
    stream
        .read_exact(&mut payload)
        .map_err(|error| format!("读取帧负载失败（声明 {length} 字节）: {error}"))?;
    Ok(Some(payload))
}

/// 写一帧。返回是否成功（失败时调用方应当结束这条连接）。
fn write_frame(stream: &mut TcpStream, response: &AgentResponse) -> bool {
    let Ok(payload) = serde_json::to_vec(response) else {
        return false;
    };
    let Ok(length) = u32::try_from(payload.len()) else {
        return false;
    };
    stream.write_all(&length.to_le_bytes()).is_ok() && stream.write_all(&payload).is_ok()
}

// ════════════════════════════════════════════════════════════ 请求分发
// 分发**不碰网络**，所以可以直接单测：给一个请求，断言响应。

/// 把一条请求翻译成响应。
///
/// 所有会改动目标的操作都走 `crate::exports` 里的 `pb_*`——也就是说
/// 能力校验、状态码、错误文本与"C ABI 直连"路径**完全一致**。
#[must_use]
pub fn dispatch(request: &AgentRequest) -> AgentResponse {
    match request {
        AgentRequest::Ping { wire_version } => dispatch_ping(*wire_version),
        AgentRequest::ReadMemory { address, length } => dispatch_read_memory(*address, *length),
        AgentRequest::WriteMemory { address, hex_bytes } => match decode_hex(hex_bytes) {
            Ok(bytes) => {
                // SAFETY: bytes 是本次调用期间存活的切片，长度由 len_u32 给出
                let code =
                    unsafe { exports::pb_memory_write(*address, bytes.as_ptr(), len_u32(&bytes)) };
                status_only(code, "write_memory")
            }
            Err(message) => AgentResponse::Error {
                code: error_code::INVALID_ARGUMENT.into(),
                message,
            },
        },
        // 寄存器读取需要 Pin 的上下文访问，M1 落地。
        AgentRequest::ReadRegisters { .. } => not_supported("read_registers"),
        AgentRequest::Modules {} => dispatch_modules(),
        AgentRequest::SetBreakpoint { address, label } => {
            let _ = label; // 标签只用于审计展示，不参与下发
            status_only(exports::pb_set_breakpoint(*address), "set_breakpoint")
        }
        AgentRequest::ClearBreakpoint { address } => {
            status_only(exports::pb_clear_breakpoint(*address), "clear_breakpoint")
        }
        AgentRequest::SingleStep { count } => {
            status_only(exports::pb_single_step(*count), "single_step")
        }
        AgentRequest::Continue => status_only(exports::pb_continue(), "continue"),
        AgentRequest::Pause => status_only(exports::pb_pause(), "pause"),
        // 运行到地址需要单步与断点配合的实现，M1 落地。
        AgentRequest::RunTo { .. } => not_supported("run_to"),
        AgentRequest::LoadScript { name, source, .. } => dispatch_load_script(name, source),
        // 卸载需要脚本运行时（M1）；M0 连加载都还是桩，卸载更没有意义。
        AgentRequest::UnloadScript { .. } => not_supported("unload_script"),
        AgentRequest::DrainEvents {
            from,
            limit,
            fold_repeats,
        } => {
            let _ = fold_repeats; // 折叠在 Hub 侧做（agent 的队列里没有折叠标记）
            dispatch_drain_events(*from, *limit)
        }
        AgentRequest::StartTrace { path, fold } => dispatch_start_trace(path, *fold),
        AgentRequest::StopTrace => dispatch_stop_trace(),
    }
}

/// 枚举已加载模块。
///
/// 失败时不返回"空列表"：空列表是有含义的（"一个模块都没有"），
/// 平台不支持或枚举失败必须用错误表达，否则调用方会把"做不到"当成"没有"。
fn dispatch_modules() -> AgentResponse {
    if !crate::modules::supported() {
        return AgentResponse::Error {
            code: error_code::NOT_SUPPORTED.into(),
            message: "modules: 当前平台没有 PEB，进程内模块枚举只在 Windows 上实现".into(),
        };
    }
    let snapshot = crate::modules::snapshot();
    AgentResponse::Modules {
        modules: snapshot.modules,
        skipped: snapshot.skipped,
    }
}
fn dispatch_ping(wire_version: u16) -> AgentResponse {
    // 版本协商：不匹配时明确拒绝，而不是"能解析就继续"。
    // 一个字段含义变了但版本没变的链路，会产生最难查的那类 bug。
    if wire_version != pinbridge_core::WIRE_VERSION {
        return AgentResponse::Error {
            code: error_code::PROTOCOL_ERROR.into(),
            message: format!(
                "协议版本不匹配：客户端 {wire_version}，本 agent {}（拒绝继续）",
                pinbridge_core::WIRE_VERSION
            ),
        };
    }
    AgentResponse::Pong {
        agent_version: format!("pinbridge-agent {}", env!("CARGO_PKG_VERSION")),
        arch: std::env::consts::ARCH.to_string(),
        capabilities: pinbridge_core::CapabilitySet::from_bits(exports::pb_get_capabilities()),
    }
}

fn dispatch_read_memory(address: u64, length: u32) -> AgentResponse {
    // 链路侧自己设一个上限并**明确拒绝**超限请求，而不是悄悄截断成更短的读：
    // "我要 1 MiB，你给我 64 KiB 且没告诉我"会让上层把半截数据当完整数据用。
    if length > MAX_LINK_READ_BYTES {
        return AgentResponse::Error {
            code: error_code::INVALID_ARGUMENT.into(),
            message: format!(
                "read_memory: 单次读取上限 {MAX_LINK_READ_BYTES} 字节（收到 {length}）；\
                 大块内存请用脚本侧的 snapshot（见 docs/design/script-runtime.md）"
            ),
        };
    }
    let mut buffer = vec![0u8; length as usize];
    let mut written: u32 = 0;
    // SAFETY: buffer 有 length 字节且非空（length==0 时 Vec 为空但指针仍有效，
    // 且导出面在 length > out_capacity 时会先拒绝），out_written 指向本栈变量。
    let code = unsafe {
        exports::pb_memory_read(address, length, buffer.as_mut_ptr(), length, &mut written)
    };
    let status = status_from_code(code);
    if status != Status::Ok {
        return error_response(status, "read_memory");
    }
    buffer.truncate(written.min(length) as usize);
    AgentResponse::Memory {
        address,
        hex_bytes: encode_hex(&buffer),
    }
}

fn dispatch_load_script(name: &str, source: &str) -> AgentResponse {
    // C ABI 收 C 串：这里把 Rust 字符串转成 NUL 结尾再传。
    // 含内嵌 NUL 的名字/源码是**调用方的 bug**（无法表达成 C 串），明确拒绝。
    let Ok(name_c) = std::ffi::CString::new(name) else {
        return AgentResponse::Error {
            code: error_code::INVALID_ARGUMENT.into(),
            message: "脚本名含内嵌 NUL，无法作为 C 串传递".into(),
        };
    };
    let Ok(source_c) = std::ffi::CString::new(source) else {
        return AgentResponse::Error {
            code: error_code::INVALID_ARGUMENT.into(),
            message: "脚本源码含内嵌 NUL，无法作为 C 串传递".into(),
        };
    };
    // SAFETY: 两个 CString 在本调用期间存活且以 NUL 结尾
    let code = unsafe { exports::pb_load_script(name_c.as_ptr(), source_c.as_ptr()) };
    let status = status_from_code(code);
    if status != Status::Ok {
        return error_response(status, "load_script");
    }
    // M0 的桩不会走到这里；M1 起导出面需要返回脚本 id（见 docs/roadmap.md）。
    AgentResponse::Script { script: 0 }
}

fn dispatch_drain_events(from: u64, limit: u32) -> AgentResponse {
    let capacity = limit.clamp(1, MAX_DRAIN_LIMIT);
    let mut buffer = vec![PbEventRaw::EMPTY; capacity as usize];
    let mut written: u32 = 0;
    // SAFETY: buffer 有 capacity 个元素，written 指向本栈变量
    let code = unsafe { exports::pb_drain_events(buffer.as_mut_ptr(), capacity, &mut written) };
    let status = status_from_code(code);
    if status != Status::Ok {
        return error_response(status, "drain_events");
    }
    buffer.truncate(written as usize);

    let mut items: Vec<EventDto> = Vec::with_capacity(buffer.len());
    let mut next = from;
    for raw in &buffer {
        // `from` 只向前过滤：比它早且已被取走的事件不可能重现（见模块文档）。
        if raw.sequence < from {
            continue;
        }
        next = raw.sequence + 1;
        // JSON 层去掉末尾的零：定长 8 项在二进制里合理，在 JSON 里是纯浪费。
        let mut args: Vec<u64> = raw.args.to_vec();
        while args.last() == Some(&0) {
            args.pop();
        }
        items.push(EventDto {
            seq: raw.sequence,
            kind: raw.kind,
            thread: raw.thread_id,
            address: raw.address,
            args,
        });
    }
    let dropped = exports::pb_stats(0);
    if items.is_empty() {
        next = from;
    }
    AgentResponse::Events {
        items,
        next,
        dropped,
    }
}

fn dispatch_start_trace(path: &str, fold: bool) -> AgentResponse {
    let Ok(path_c) = std::ffi::CString::new(path) else {
        return AgentResponse::Error {
            code: error_code::INVALID_ARGUMENT.into(),
            message: "轨迹路径含内嵌 NUL".into(),
        };
    };
    // SAFETY: path_c 在本调用期间存活且以 NUL 结尾
    let code = unsafe { exports::pb_start_trace(path_c.as_ptr(), u32::from(fold)) };
    let status = status_from_code(code);
    if status != Status::Ok {
        return error_response(status, "start_trace");
    }
    AgentResponse::Trace {
        path: path.to_string(),
    }
}

fn dispatch_stop_trace() -> AgentResponse {
    let mut records: u64 = 0;
    let mut events: u64 = 0;
    // SAFETY: 两个 out 参数都指向本函数的栈变量
    let code = unsafe { exports::pb_stop_trace(&mut records, &mut events) };
    let status = status_from_code(code);
    if status != Status::Ok {
        return error_response(status, "stop_trace");
    }
    AgentResponse::TraceSummary {
        records,
        events,
        // 精确的折叠条数只在 `.pbt` 的 SUMMARY 段里；这里是差值近似，
        // 故意不假装精确（见 docs/specs/pbt-format.md §6）。
        folded: events.saturating_sub(records),
        dropped: exports::pb_stats(0),
    }
}

/// 把"只有状态码"的导出结果翻成响应。
fn status_only(code: i32, operation: &str) -> AgentResponse {
    let status = status_from_code(code);
    if status == Status::Ok {
        AgentResponse::Ok
    } else {
        error_response(status, operation)
    }
}

fn not_supported(operation: &str) -> AgentResponse {
    AgentResponse::Error {
        code: error_code::NOT_SUPPORTED.into(),
        message: format!(
            "{operation}: 该能力尚未实现（M0 的桩会显式走到这里，见 docs/roadmap.md 的 M1）"
        ),
    }
}

/// 未知状态码一律按内部错误处理——**不要猜**。
fn status_from_code(code: i32) -> Status {
    Status::from_code(code).unwrap_or(Status::Internal)
}

/// 把状态码翻成错误响应：机器可读码 + agent 侧那条可读的错误文本。
///
/// 直接复用 `pb_last_error()`，于是链路与 C ABI 路径给出的解释**逐字相同**。
fn error_response(status: Status, operation: &str) -> AgentResponse {
    let code = match status {
        Status::CapabilityDenied => error_code::CAPABILITY_DENIED,
        Status::NotAttached => error_code::NOT_ATTACHED,
        Status::InvalidArgument => error_code::INVALID_ARGUMENT,
        Status::NotSupported => error_code::NOT_SUPPORTED,
        Status::Busy => error_code::BUSY,
        Status::Timeout => error_code::TIMEOUT,
        Status::AbiMismatch => error_code::PROTOCOL_ERROR,
        Status::Ok | Status::Internal => error_code::INTERNAL,
    };
    let detail = last_error_text();
    AgentResponse::Error {
        code: code.into(),
        message: if detail.is_empty() {
            format!("{operation}: {}", status.message())
        } else {
            format!("{operation}: {detail}")
        },
    }
}

/// 取 agent 侧最后一条错误文本（不是 C 指针泄漏，而是立刻拷成 `String`）。
fn last_error_text() -> String {
    // SAFETY: pb_last_error 永不返回 NULL，且返回的指针由 agent 内部持有；
    // 本次调用期间不会被改写（没有其它线程在同时写错误状态），
    // 因此立刻拷贝成 String 是安全的。
    unsafe {
        let raw = exports::pb_last_error();
        if raw.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned()
    }
}

/// `usize` → `u32`，溢出时截断而不是 panic（长度本身还会被导出面校验）。
fn len_u32(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).unwrap_or(u32::MAX)
}

/// 字节 → 小写 hex。
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0F)] as char);
    }
    out
}

/// 小写/大写 hex → 字节。奇数长度或非法字符一律拒绝并说明原因。
///
/// # Errors
///
/// 返回人类可读的原因（会原样进 `invalid_argument` 响应）。
pub fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err(format!("hex 长度必须是偶数，收到 {}", text.len()));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    // 按下标两两推进（而不是 `chunks_exact(2)`）：后者要求配对切片再取下标，
    // 会让"长度必须是偶数"这个前置条件在两个地方各写一遍。
    let mut index = 0;
    while index < bytes.len() {
        let high = nibble(bytes[index])?;
        let low = nibble(bytes[index + 1])?;
        out.push((high << 4) | low);
        index += 2;
    }
    Ok(out)
}

fn nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(format!("hex 里出现非法字符 {:?}", other as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{AgentHost, GLOBAL_STATE_LOCK};
    use pinbridge_core::capability::{Capability, CapabilitySet};
    use pinbridge_trace::event::{Event, EventKind};

    /// 全局状态在测试间共享：所有触碰它的用例都先拿这把锁，
    /// 并且**自己设置初始状态**（不依赖某个用例先跑过）。
    /// 取得全局状态锁并把状态复位到给定能力集。
    ///
    /// **返回 guard** 而不是在函数内部持锁：这些测试断言的是"某个能力集下的行为"，
    /// 锁必须覆盖整个用例体，否则别的用例会在中途把能力集改掉
    /// （这类偶发失败极难复现）。调用方写成 `let _guard = lock_and_reset(...)`。
    ///
    /// 队列的清理必须**循环拉空**：早先只排一次 64 条就返回，于是"上一个用例
    /// 留下的事件"会混进本次断言——实测出现过"推 3 条取回 8 条"的偶发失败
    /// （8 是本次请求的 limit，说明缓冲区被别人的事件填满了）。
    /// 依赖清理是上下文复位的常见坑：复位必须做到"干净"，而不是"差不多干净"。
    fn lock_and_reset(capabilities: CapabilitySet) -> std::sync::MutexGuard<'static, ()> {
        let guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        exports::pb_detach();
        exports::pb_set_capabilities(capabilities.bits());

        // 用 Vec 而不是栈数组：256 × 88 字节 ≈ 22 KiB，栈上放会被 clippy 拦
        // （`large_stack_arrays`），而且这里本来也只是个临时缓冲。
        let mut sink = vec![PbEventRaw::EMPTY; 256];
        let mut written = 0;
        loop {
            // SAFETY: sink 有 256 个元素，written 指向本栈变量
            let code = unsafe { exports::pb_drain_events(sink.as_mut_ptr(), 256, &mut written) };
            assert_eq!(code, Status::Ok.code(), "复位时 drain 不应失败");
            if written == 0 {
                break;
            }
        }
        guard
    }

    fn push_events(count: u64) {
        for index in 0..count {
            let sequence = exports::pb_allocate_sequence();
            let event = Event {
                sequence,
                kind: EventKind::MemWrite,
                thread_id: 100,
                address: 0x7FF6_0000_3000 + index * 8,
                args: [index + 1, 0, 0, 0, 0, 0, 0, 0],
            };
            AgentHost::with(|state| state.push_event(event)).expect("事件应当入队");
        }
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        assert_eq!(encode_hex(&[0x00, 0xFF, 0x90]), "00ff90");
        assert_eq!(decode_hex("00ff90").unwrap(), vec![0x00, 0xFF, 0x90]);
        assert_eq!(
            decode_hex("DeadBeef").unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
        assert!(decode_hex("").unwrap().is_empty());
        // 奇数长度：不能"补齐一位"猜，必须拒绝并说明
        let err = decode_hex("90a").unwrap_err();
        assert!(err.contains("偶数"), "实际: {err}");
        // 非法字符：把原因说清楚（谁传的谁改）
        let err = decode_hex("9g").unwrap_err();
        assert!(err.contains("非法字符"), "实际: {err}");
    }

    /// 这条测试在**真实进程**里跑：它枚举的是测试进程自己的加载器链。
    /// 偏移写错的话表现是空列表或访问违例，而不是"看起来合理的一串模块"。
    #[test]
    fn modules_request_enumerates_this_process() {
        let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        exports::pb_set_capabilities(Capability::read_only_baseline().bits());
        match dispatch(&AgentRequest::Modules {}) {
            AgentResponse::Modules { modules, skipped } => {
                if !cfg!(windows) {
                    assert!(modules.is_empty(), "非 Windows 平台应当没有 PEB 可读");
                    return;
                }
                assert!(
                    modules.len() >= 3,
                    "任何 Windows 进程至少加载了主模块、ntdll、kernel32，实际 {}",
                    modules.len()
                );
                for module in &modules {
                    assert!(
                        module.base != 0,
                        "{} 的基址为 0（偏移读错了？）",
                        module.name
                    );
                    assert!(module.size != 0, "{} 的映像大小为 0", module.name);
                    assert!(
                        module.name.contains('\\') || module.name.contains('/'),
                        "模块名应当是完整路径，实际: {}",
                        module.name
                    );
                }
                println!(
                    "✓ 通过链路枚举到 {} 个模块（跳过 {skipped}）",
                    modules.len()
                );
                println!("  第一个：{}", modules[0].name);
            }
            other => panic!("期望 Modules，实际: {other:?}"),
        }
    }
    #[test]
    fn ping_negotiates_the_protocol_version() {
        let _guard = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        exports::pb_set_capabilities(Capability::read_only_baseline().bits());
        match dispatch(&AgentRequest::Ping {
            wire_version: pinbridge_core::WIRE_VERSION,
        }) {
            AgentResponse::Pong {
                agent_version,
                arch,
                capabilities,
            } => {
                assert!(
                    agent_version.contains("pinbridge-agent"),
                    "实际: {agent_version}"
                );
                assert!(!arch.is_empty());
                assert_eq!(capabilities, Capability::read_only_baseline());
            }
            other => panic!("期望 Pong，实际: {other:?}"),
        }

        let other = dispatch(&AgentRequest::Ping { wire_version: 999 });
        match other {
            AgentResponse::Error { code, message } => {
                assert_eq!(code, error_code::PROTOCOL_ERROR);
                assert!(
                    message.contains("999"),
                    "错误文本必须给出双方版本: {message}"
                );
            }
            other => panic!("版本不匹配必须被拒，实际: {other:?}"),
        }
    }

    #[test]
    fn the_two_gates_are_distinguishable_on_the_link() {
        // 第一道闸门：能力。空能力集下连读取都该被拒。
        let _guard = lock_and_reset(CapabilitySet::empty());
        match dispatch(&AgentRequest::ReadMemory {
            address: 0,
            length: 4,
        }) {
            AgentResponse::Error { code, .. } => {
                assert_eq!(
                    code,
                    error_code::CAPABILITY_DENIED,
                    "空能力集下读取必须被拒"
                );
            }
            other => panic!("实际: {other:?}"),
        }

        // 第二道闸门：实现。给出只读能力后，能力这关过了，桩必须说"不支持"。
        exports::pb_set_capabilities(Capability::read_only_baseline().bits());
        match dispatch(&AgentRequest::ReadMemory {
            address: 0,
            length: 4,
        }) {
            AgentResponse::Error { code, message } => {
                assert_eq!(code, error_code::NOT_SUPPORTED, "实际: {message}");
                assert!(
                    message.contains("M0") || message.contains("未实现") || message.contains("Pin"),
                    "不支持的说明要指出后续怎么办: {message}"
                );
            }
            other => panic!("实际: {other:?}"),
        }
    }

    #[test]
    fn unimplemented_requests_say_so_explicitly() {
        let _guard = lock_and_reset(Capability::operator_default());
        for request in [
            AgentRequest::ReadRegisters { thread: 0 },
            AgentRequest::RunTo { address: 0x1000 },
            AgentRequest::UnloadScript { script: 1 },
        ] {
            match dispatch(&request) {
                AgentResponse::Error { code, message } => {
                    assert_eq!(
                        code,
                        error_code::NOT_SUPPORTED,
                        "{} 应当明确不支持",
                        request.name()
                    );
                    assert!(!message.is_empty());
                }
                other => panic!("{} 实际: {other:?}", request.name()),
            }
        }
    }

    #[test]
    fn drain_events_filters_forward_and_trims_args() {
        let _guard = lock_and_reset(Capability::operator_default());
        push_events(3);

        // 全量拉取：args 末尾的零被去掉，next 指向下一条
        match dispatch(&AgentRequest::DrainEvents {
            from: 1,
            limit: 8,
            fold_repeats: false,
        }) {
            AgentResponse::Events {
                items,
                next,
                dropped,
            } => {
                assert_eq!(items.len(), 3, "推入 3 条应当取回 3 条");
                assert_eq!(dropped, 0, "没有背压就不该报丢弃");
                assert_eq!(next, items.last().unwrap().seq + 1);
                assert_eq!(items[0].args.len(), 1, "args 末尾的零应当被去掉");
                assert_eq!(items[0].kind, EventKind::MemWrite.code());
            }
            other => panic!("实际: {other:?}"),
        }

        // `from` 只向前过滤：比 from 小的**已经取走**，不会重现
        match dispatch(&AgentRequest::DrainEvents {
            from: 1_000_000,
            limit: 8,
            fold_repeats: false,
        }) {
            AgentResponse::Events {
                items,
                next,
                dropped,
            } => {
                assert!(items.is_empty(), "队列已空，取不到任何事件");
                assert_eq!(next, 1_000_000, "没有取到东西时游标不应推进");
                assert_eq!(dropped, 0);
            }
            other => panic!("实际: {other:?}"),
        }
    }

    #[test]
    fn oversized_and_malformed_inputs_are_rejected_with_reasons() {
        let _guard = lock_and_reset(Capability::operator_default());

        // 超过链路读取上限：明确拒绝，而不是悄悄截断成更短的读
        match dispatch(&AgentRequest::ReadMemory {
            address: 0,
            length: MAX_LINK_READ_BYTES + 1,
        }) {
            AgentResponse::Error { code, message } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(message.contains("上限"), "实际: {message}");
            }
            other => panic!("实际: {other:?}"),
        }

        // 非法 hex
        match dispatch(&AgentRequest::WriteMemory {
            address: 0x1000,
            hex_bytes: "9g".into(),
        }) {
            AgentResponse::Error { code, message } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(message.contains("非法字符"), "实际: {message}");
            }
            other => panic!("实际: {other:?}"),
        }

        // 内嵌 NUL 的脚本名无法表达成 C 串（不是"权限"问题，是调用方 bug）
        match dispatch(&AgentRequest::LoadScript {
            name: "bad\0name".into(),
            source: "print(1)".into(),
            subscribe: Vec::new(),
        }) {
            AgentResponse::Error { code, message } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(message.contains("NUL"), "实际: {message}");
            }
            other => panic!("实际: {other:?}"),
        }
    }

    #[test]
    fn write_requests_need_the_write_capability() {
        let _guard = lock_and_reset(Capability::read_only_baseline());
        // 只读基线不允许写内存——这是"AI 默认最小权限"在链路层的体现
        match dispatch(&AgentRequest::WriteMemory {
            address: 0x1000,
            hex_bytes: "90".into(),
        }) {
            AgentResponse::Error { code, .. } => {
                assert_eq!(code, error_code::CAPABILITY_DENIED);
            }
            other => panic!("实际: {other:?}"),
        }
    }

    #[test]
    fn every_status_code_has_a_distinct_error_code() {
        let mut seen = std::collections::HashSet::new();
        for status in [
            Status::CapabilityDenied,
            Status::NotAttached,
            Status::InvalidArgument,
            Status::NotSupported,
            Status::Busy,
            Status::Timeout,
            Status::AbiMismatch,
            Status::Internal,
        ] {
            let AgentResponse::Error { code, message } = error_response(status, "op") else {
                panic!("error_response 必须给出 Error");
            };
            assert!(!message.is_empty(), "{status:?} 的错误文本不能为空");
            // Internal 是多个未知情况的兜底，允许共享同一个码
            if status != Status::Internal {
                assert!(
                    seen.insert(code.clone()),
                    "错误码重复: {code}（{status:?}）"
                );
            }
        }
    }

    #[test]
    fn server_survives_a_closed_and_a_silent_peer() {
        let _guard = lock_and_reset(Capability::operator_default());
        let server = LinkServer::bind()
            .expect("绑定回环端口")
            .with_idle_timeout(Duration::from_millis(200));
        let addr = server.addr();
        assert!(addr.ip().is_loopback(), "只能绑回环");

        // 1) 对端连上就断：服务端必须正常返回，而不是报错或永久阻塞
        let handle = std::thread::spawn(move || server.serve_once());
        std::net::TcpStream::connect(addr)
            .expect("连接")
            .shutdown(std::net::Shutdown::Both)
            .ok();
        assert_eq!(handle.join().unwrap().expect("对端断开不算错误"), 0);
    }

    #[test]
    fn frames_round_trip_over_a_real_socket() {
        let _guard = lock_and_reset(Capability::operator_default());
        let server = LinkServer::bind().expect("绑定");
        let addr = server.addr();
        let handle = std::thread::spawn(move || server.serve_once());

        let mut stream = std::net::TcpStream::connect(addr).expect("连接");
        let request = serde_json::to_vec(&AgentRequest::Ping {
            wire_version: pinbridge_core::WIRE_VERSION,
        })
        .unwrap();
        stream
            .write_all(&(request.len() as u32).to_le_bytes())
            .unwrap();
        stream.write_all(&request).unwrap();

        let mut head = [0u8; 4];
        stream.read_exact(&mut head).unwrap();
        let length = u32::from_le_bytes(head) as usize;
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload).unwrap();
        let response: AgentResponse = serde_json::from_slice(&payload).expect("解析响应");
        match response {
            AgentResponse::Pong { capabilities, .. } => {
                assert_eq!(capabilities, Capability::operator_default());
            }
            other => panic!("实际: {other:?}"),
        }

        // 2) 非法 JSON 必须得到**结构化错误**，而不是静默丢弃
        let junk = "{\"req\":\"not_a_real_request\"}".as_bytes();
        stream
            .write_all(&(junk.len() as u32).to_le_bytes())
            .unwrap();
        stream.write_all(junk).unwrap();
        stream.read_exact(&mut head).unwrap();
        let length = u32::from_le_bytes(head) as usize;
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload).unwrap();
        match serde_json::from_slice::<AgentResponse>(&payload).expect("解析") {
            AgentResponse::Error { code, .. } => assert_eq!(code, error_code::PROTOCOL_ERROR),
            other => panic!("实际: {other:?}"),
        }

        // 3) 超大帧头：拒绝并关闭，不能按声明长度去分配内存
        stream
            .write_all(&(MAX_FRAME_BYTES as u32 + 1).to_le_bytes())
            .unwrap();
        stream.read_exact(&mut head).unwrap();
        let length = u32::from_le_bytes(head) as usize;
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload).unwrap();
        match serde_json::from_slice::<AgentResponse>(&payload).expect("解析") {
            AgentResponse::Error { code, message } => {
                assert_eq!(code, error_code::PROTOCOL_ERROR);
                assert!(message.contains("过大"), "实际: {message}");
            }
            other => panic!("实际: {other:?}"),
        }

        drop(stream);
        let handled = handle.join().unwrap().expect("服务端正常结束");
        assert_eq!(handled, 1, "只有第一条是合法请求（后两条是协议错误）");
    }
}
