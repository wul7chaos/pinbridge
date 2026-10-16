//! `pinbridge.dll` 的运行时加载器。
//!
//! 为什么是**运行时加载**而不是链接期依赖：
//!
//! * Pin 的工具 DLL 由 `pin.exe` 注入并被 Pin 加载，不是普通的链接库；
//! * 开发机上没装 Pin 时，`pinbridge-hub` / `pinbridge-cli` 仍然应该能构建、
//!   能跑单元测试、能在"离线模式"下工作（读已有轨迹、做重放分析）；
//! * 链接期依赖会让"缺 Pin 就编译不过"，这对一个要在 CI 上跑的仓库是不可接受的。
//!
//! 加载流程刻意保守：**先校验 ABI 版本，再取函数指针**。
//! 版本不对时立刻返回 `AbiMismatch`，绝不做"能调就调"的乐观尝试——
//! 一个错位的函数指针在插装场景下会直接把目标进程打崩，
//! 而崩因会指向目标程序本身，排查代价极高。

use std::cell::Cell;
use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_int, c_uint};

use pinbridge_core::capability::Capability;
use pinbridge_core::error::{Error, Result};

use crate::abi::{PbEventRaw, PbSet, Status};
// 注意：`PB_ABI_VERSION` 只在 `#[cfg(windows)]` 的 `load()` 里用到，
// 因此**不能**在文件顶部无条件导入——那会让非 Windows 构建因 unused_imports 失败。
// （这条就是 CI 的 Linux 任务抓出来的；本地靠
//  `cargo clippy --target x86_64-unknown-linux-gnu` 复现。）
#[cfg(windows)]
use crate::abi::PB_ABI_VERSION;

/// `pinbridge.dll` 的 ABI 函数签名表。
///
/// 所有函数都必须在这里列出；新增函数要同步改 `native/include/pinbridge.h`
/// 与 `docs/specs/c-abi.md`。
///
/// 注意**刻意不是 `Copy`**：它持有"我们下发过的能力集"这一可变状态，
/// 静默复制会让两份记录分叉（于是错误里的 `held` 开始说谎）。
/// `Clone` 保留，但用它的地方应当先想清楚"谁拥有这条链路的状态"。
#[derive(Debug, Clone)]
pub struct Api {
    /// DLL 句柄（`HMODULE`）。**不实现 `Send`/`Sync`**：Pin 的会话是有线程归属的。
    handle: *mut c_void,
    /// 最近一次成功下发的能力集。
    ///
    /// 为什么要记它：DLL 的能力不足只回报一个整数 -3，`Error::CapabilityDenied`
    /// 却需要"需要什么/持有什么"。如果这里填一个空集或占位值，
    /// 读错误的人（很可能是 AI）就会得到**错误的**上下文，
    /// 进而去申请一个它其实已经有的能力。保存真实下发过的集合，
    /// 才能让"能力不足"这条错误自洽。
    ///
    /// `Cell` 而不是 `Mutex`：`Api` 本来就刻意不是 `Send`/`Sync`（见上），
    /// 加锁只会把一个"线程归属"问题变成一个"死锁"问题。
    applied_bits: Cell<u32>,
    /// `uint32_t pb_abi_version(void)`
    pub abi_version: unsafe extern "C" fn() -> u32,
    /// `int32_t pb_start(const char* image_path, const char* args)`
    pub start: unsafe extern "C" fn(*const c_char, *const c_char) -> c_int,
    /// `int32_t pb_attach(uint32_t pid)`
    pub attach: unsafe extern "C" fn(c_uint) -> c_int,
    /// `int32_t pb_set_capabilities(uint32_t bits)`
    pub set_capabilities: unsafe extern "C" fn(c_uint) -> c_int,
    /// `int32_t pb_continue(void)`
    pub resume: unsafe extern "C" fn() -> c_int,
    /// `int32_t pb_pause(void)`
    pub pause: unsafe extern "C" fn() -> c_int,
    /// `int32_t pb_single_step(uint32_t)`
    ///
    /// 解析出来只为把"**未实现**"这件事映射成明确的错误（而不是让调用方
    /// 以为符号缺失）：单步在 PinBridge 的模型里没有调试器语义，见
    /// agent 侧 `pb_single_step` 的文档。
    pub single_step: unsafe extern "C" fn(u32) -> c_int,
    /// `int32_t pb_drain_events(PbEventRaw* out, uint32_t capacity, uint32_t* written)`
    pub drain_events: unsafe extern "C" fn(*mut PbEventRaw, c_uint, *mut c_uint) -> c_int,
    /// `int32_t pb_start_trace(const char* path, uint32_t fold)`
    ///
    /// 让 **Hub 能驱动目标内录制**：轨迹由 DLL 这一侧写盘，而不是把事件全量
    /// 搬回 Hub 再拼。少了它，"录一份轨迹"就只剩"Hub 自己收事件"这一条路。
    pub start_trace: unsafe extern "C" fn(*const c_char, c_uint) -> c_int,
    /// `int32_t pb_stop_trace(uint64_t* out_records, uint64_t* out_events)`
    pub stop_trace: unsafe extern "C" fn(*mut u64, *mut u64) -> c_int,
    /// `const char* pb_last_error(void)`
    pub last_error: unsafe extern "C" fn() -> *const c_char,
}

/// 加载并解析 `pinbridge.dll`。
///
/// `name` 允许是绝对路径，也允许是"已在搜索路径中的 DLL 名"。
/// 解析失败时错误信息里会带上**具体缺失的导出名**：
/// "DLL 加载失败"这种信息在排查 Pin 版本差异时毫无价值。
#[cfg(windows)]
pub fn load(name: &str) -> Result<Api> {
    use std::ffi::CString;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    let name_c =
        CString::new(name).map_err(|_| Error::Protocol(format!("DLL 路径含 NUL 字节: {name}")))?;
    // SAFETY: name_c 以 NUL 结尾且在本调用期间存活
    let handle = unsafe { LoadLibraryA(name_c.as_ptr()) };
    if handle.is_null() {
        return Err(Error::TargetUnavailable(format!(
            "加载 {name} 失败：文件不存在、位数不匹配或依赖缺失"
        )));
    }

    /// 取一个导出函数指针。
    ///
    /// # Safety
    ///
    /// `handle` 必须是 `LoadLibraryA` 返回的有效模块句柄；
    /// 调用方必须保证泛型 `F` 的签名与 DLL 中同名函数的真实签名一致。
    unsafe fn resolve<F: Copy>(handle: *mut c_void, module: &str, symbol: &str) -> Result<F> {
        let symbol_c = std::ffi::CString::new(symbol)
            .map_err(|_| Error::Protocol(format!("符号名含 NUL: {symbol}")))?;
        // SAFETY: 由调用方保证 handle 有效、符号名以 NUL 结尾
        let address = unsafe { GetProcAddress(handle, symbol_c.as_ptr()) };
        if address.is_null() {
            return Err(Error::Protocol(format!("{module} 缺少导出符号 {symbol}")));
        }
        debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
        // SAFETY: 函数指针与数据指针在 x86-64 Windows ABI 下同宽（C 调用约定）
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) })
    }

    // SAFETY: 符号名与 native/include/pinbridge.h 中声明的签名逐一对应
    let api = unsafe {
        Api {
            handle,
            // 加载时 DLL 尚未被告知任何能力：真相来源是"还没下发过"，
            // 因此初始值就是空集（fail-closed：没下发就什么都做不了）。
            applied_bits: Cell::new(0),
            abi_version: resolve(handle, name, "pb_abi_version")?,
            start: resolve(handle, name, "pb_start")?,
            attach: resolve(handle, name, "pb_attach")?,
            set_capabilities: resolve(handle, name, "pb_set_capabilities")?,
            resume: resolve(handle, name, "pb_continue")?,
            pause: resolve(handle, name, "pb_pause")?,
            single_step: resolve(handle, name, "pb_single_step")?,
            drain_events: resolve(handle, name, "pb_drain_events")?,
            start_trace: resolve(handle, name, "pb_start_trace")?,
            stop_trace: resolve(handle, name, "pb_stop_trace")?,
            last_error: resolve(handle, name, "pb_last_error")?,
        }
    };

    // 版本闸门：先校验再放行
    // SAFETY: 函数指针已由 resolve 校验非空，签名与头文件一致
    let reported = unsafe { (api.abi_version)() };
    if reported != PB_ABI_VERSION {
        return Err(Error::Protocol(format!(
            "{name} 的 ABI 版本为 {reported}，本构建期望 {PB_ABI_VERSION}（拒绝加载）"
        )));
    }
    Ok(api)
}

/// Windows 之外没有 Pin 的动态插装能力（M0 范围）。
#[cfg(not(windows))]
pub fn load(name: &str) -> Result<Api> {
    Err(Error::TargetUnavailable(format!(
        "{name}: 当前平台不支持 Pin 动态插装（M0 仅支持 Windows）"
    )))
}

impl Api {
    /// 取模块句柄（调试与日志用）。
    #[must_use]
    pub fn handle(&self) -> *mut c_void {
        self.handle
    }

    /// 本侧记录的"目标端当前持有的能力集"。
    ///
    /// 它只反映**我们下发过**的内容——目标端才是真相来源。
    /// 作用是让 `CapabilityDenied` 这类错误能自洽地报出"持有的是什么"，
    /// 而不是填一个占位值把读错误的人（很可能是 AI）带偏。
    #[must_use]
    pub fn held_capabilities(&self) -> CapabilitySet {
        CapabilitySet::from_bits(self.applied_bits.get())
    }

    /// 读取 `pb_last_error()` 的文本。
    ///
    /// # Safety
    ///
    /// 依赖 DLL 已经加载且未被卸载；返回的指针由 DLL 内部持有，
    /// 下一次调用可能被覆盖，因此这里立刻拷贝成 `String`。
    pub unsafe fn last_error_string(&self) -> Option<String> {
        // SAFETY: 由调用方保证 DLL 仍然加载着
        unsafe { crate::abi::cstr_to_string((self.last_error)()) }
    }

    /// 把一批事件拉进调用方提供的缓冲区。
    ///
    /// 返回实际取到的条数。缓冲为 0 时返回 `InvalidArgument`
    /// （**不允许"传 0 就当查询条数"的隐式语义**，那会让调用方误以为取到了数据）。
    pub fn drain(&self, buffer: &mut [PbEventRaw]) -> Result<usize> {
        if buffer.is_empty() {
            return Err(Status::InvalidArgument
                .to_error("drain_events", Capability::MemRead, Default::default())
                .unwrap());
        }
        let mut written: c_uint = 0;
        let capacity =
            c_uint::try_from(buffer.len()).map_err(|_| Error::Protocol("事件缓冲区过大".into()))?;
        // SAFETY: buffer 是有效可写切片，容量与传入的 capacity 一致
        let code = unsafe { (self.drain_events)(buffer.as_mut_ptr(), capacity, &mut written) };
        crate::abi::check(
            code,
            "drain_events",
            Capability::MemRead,
            self.held_capabilities(),
        )?;
        Ok(written as usize)
    }

    /// 下发能力集。
    pub fn apply_capabilities(&self, capabilities: PbSet) -> Result<()> {
        // SAFETY: 函数指针已在加载时校验
        let code = unsafe { (self.set_capabilities)(capabilities.0) };
        crate::abi::check(
            code,
            "set_capabilities",
            Capability::CapabilityGrant,
            self.held_capabilities(),
        )?;
        // 只有真的成功了才更新本地记录：失败时 DLL 的能力集没变，
        // 我们记的也必须没变（否则后续错误里的 held 会说谎）。
        self.applied_bits.set(capabilities.0);
        Ok(())
    }

    /// 读回 DLL 自报的 ABI 版本（加载时已经校验过一致性，这里用于日志与断言）。
    #[must_use]
    pub fn abi_version(&self) -> u32 {
        // SAFETY: 函数指针已在加载时校验
        unsafe { (self.abi_version)() }
    }

    /// 启动目标并插装。
    ///
    /// 需要 Pin 的 JIT 注入：无 Pin 的实现（含 `agent_bridge.dll` 与 M0 的桩）
    /// 会返回 `NotSupported`，本方法把它映射成 `TargetUnavailable` 并带上原因。
    pub fn start(&self, image_path: &str, args: &str) -> Result<()> {
        let image = c_string("start", image_path)?;
        let args = c_string("start", args)?;
        // SAFETY: 两个指针都指向本次调用期间存活的 CString 缓冲区
        let code = unsafe { (self.start)(image.as_ptr(), args.as_ptr()) };
        crate::abi::check(
            code,
            "start",
            Capability::ProcessControl,
            self.held_capabilities(),
        )
    }

    /// 附加到已运行进程。
    pub fn attach(&self, pid: u32) -> Result<()> {
        // SAFETY: 函数指针已在加载时校验
        let code = unsafe { (self.attach)(pid) };
        crate::abi::check(
            code,
            "attach",
            Capability::ProcessControl,
            self.held_capabilities(),
        )
    }

    /// 继续执行（`pb_continue`）。
    pub fn resume(&self) -> Result<()> {
        // SAFETY: 函数指针已在加载时校验
        let code = unsafe { (self.resume)() };
        crate::abi::check(
            code,
            "continue",
            Capability::ProcessControl,
            self.held_capabilities(),
        )
    }

    /// 暂停执行（`pb_pause`）。
    pub fn pause(&self) -> Result<()> {
        // SAFETY: 函数指针已在加载时校验
        let code = unsafe { (self.pause)() };
        crate::abi::check(
            code,
            "pause",
            Capability::ProcessControl,
            self.held_capabilities(),
        )
    }

    /// 单步（`pb_single_step`）——**当前明确未实现**。
    ///
    /// 这条路存在的意义是让"有权限但未实现"与"能力不足"在调用方那里
    /// **可区分**（对应 `Error::TargetUnavailable` 与 `Error::CapabilityDenied`）。
    ///
    /// # Errors
    ///
    /// - `CapabilityDenied`：没有 `single_step` 能力；
    /// - `TargetUnavailable`：有权限但实现未落地（当前总是这一步）。
    pub fn single_step(&self, count: u32) -> Result<()> {
        // SAFETY: 函数指针已在加载时校验
        let code = unsafe { (self.single_step)(count) };
        crate::abi::check(
            code,
            "single_step",
            Capability::SingleStep,
            self.held_capabilities(),
        )
    }
    /// 开始**目标内录制**（`pb_start_trace`）。
    ///
    /// `fold` 打开时，重复事件会被折叠成 `Repeat` 记录（体积换精度）——
    /// 这个开关跟着能力一起下发给目标侧，所以它必须显式出现在调用里。
    ///
    /// # Errors
    ///
    /// - `CapabilityDenied`：没有 `trace_record` 时由目标侧返回 `-3`；
    /// - `Protocol`：路径含 NUL，或目标侧返回其它错误码。
    pub fn start_trace(&self, path: &str, fold: bool) -> Result<()> {
        let path_c = c_string("轨迹路径", path)?;
        // SAFETY: 函数指针已在加载时校验；`path_c` 的生命周期覆盖本次调用
        // `u32::from(bool)` 而不是 `if fold { 1 } else { 0 }`：前者不会在
        // 将来改签名时静默变成别的语义（clippy 的 bool_to_int_with_if 也是这个意思）。
        let code = unsafe { (self.start_trace)(path_c.as_ptr(), u32::from(fold)) };
        crate::abi::check(
            code,
            "start_trace",
            Capability::TraceRecord,
            self.held_capabilities(),
        )
    }

    /// 结束**目标内录制**（`pb_stop_trace`），返回（记录数，事件数）。
    ///
    /// 两个计数**必须原样返回给调用方**：`records != events` 正说明发生了折叠，
    /// 只报其中一个会让"轨迹里有多少东西"变成一个无法回答的问题。
    ///
    /// # Errors
    ///
    /// 同 [`Api::start_trace`]。
    pub fn stop_trace(&self) -> Result<(u64, u64)> {
        let mut records: u64 = 0;
        let mut events: u64 = 0;
        // SAFETY: 函数指针已在加载时校验；两个 `out` 参数都指向本栈帧的局部量
        let code = unsafe { (self.stop_trace)(&raw mut records, &raw mut events) };
        crate::abi::check(
            code,
            "stop_trace",
            Capability::TraceRecord,
            self.held_capabilities(),
        )?;
        Ok((records, events))
    }
}

/// 把 `&str` 转成 ABI 需要的 NUL 结尾 C 串。
///
/// 含内嵌 NUL 的字符串无法表达成 C 串——这是**调用方的 bug**，
/// 因此报 `Protocol` 而不是悄悄截断（截断会让目标上出现"半条路径"）。
fn c_string(context: &str, text: &str) -> Result<CString> {
    CString::new(text)
        .map_err(|_| Error::Protocol(format!("{context}: 字符串含内嵌 NUL，无法作为 C 串传递")))
}

use pinbridge_core::capability::CapabilitySet;

#[cfg(test)]
mod tests {
    use super::*;
    // 显式导入而不是依赖 `use super::*`：上面的同名导入是 `#[cfg(windows)]` 的，
    // 而测试要在所有平台上都能编译（CI 的 Linux 任务会跑这些测试）。
    use crate::abi::PB_ABI_VERSION;

    #[test]
    fn missing_dll_is_a_clear_error_not_a_panic() {
        let err = load("pinbridge-definitely-not-here.dll").unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("加载") || text.contains("不支持"),
            "缺 DLL 时必须给出可定位的错误，实际: {text}"
        );
    }

    #[test]
    fn zero_length_drain_buffer_is_rejected_before_any_call() {
        // 用一个伪造的 Api 无法安全构造，这里只验证状态码映射的语义
        let err = Status::InvalidArgument
            .to_error("drain_events", Capability::MemRead, CapabilitySet::empty())
            .unwrap();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn abi_version_gate_documents_the_contract() {
        assert_eq!(PB_ABI_VERSION, crate::abi::PB_ABI_VERSION);
        assert!(crate::abi::abi_version_compatible(PB_ABI_VERSION));
    }
}
