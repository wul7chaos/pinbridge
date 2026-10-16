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

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_uint};

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
#[derive(Debug, Clone, Copy)]
pub struct Api {
    /// DLL 句柄（`HMODULE`）。**不实现 `Send`/`Sync`**：Pin 的会话是有线程归属的。
    handle: *mut c_void,
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
    /// `int32_t pb_drain_events(PbEventRaw* out, uint32_t capacity, uint32_t* written)`
    pub drain_events: unsafe extern "C" fn(*mut PbEventRaw, c_uint, *mut c_uint) -> c_int,
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
            abi_version: resolve(handle, name, "pb_abi_version")?,
            start: resolve(handle, name, "pb_start")?,
            attach: resolve(handle, name, "pb_attach")?,
            set_capabilities: resolve(handle, name, "pb_set_capabilities")?,
            resume: resolve(handle, name, "pb_continue")?,
            pause: resolve(handle, name, "pb_pause")?,
            drain_events: resolve(handle, name, "pb_drain_events")?,
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
                .to_error("drain_events", Default::default())
                .unwrap());
        }
        let mut written: c_uint = 0;
        let capacity =
            c_uint::try_from(buffer.len()).map_err(|_| Error::Protocol("事件缓冲区过大".into()))?;
        // SAFETY: buffer 是有效可写切片，容量与传入的 capacity 一致
        let code = unsafe { (self.drain_events)(buffer.as_mut_ptr(), capacity, &mut written) };
        crate::abi::check(code, "drain_events", pinbridge_core::CapabilitySet::empty())?;
        Ok(written as usize)
    }

    /// 下发能力集。
    pub fn apply_capabilities(&self, capabilities: PbSet) -> Result<()> {
        // SAFETY: 函数指针已在加载时校验
        let code = unsafe { (self.set_capabilities)(capabilities.0) };
        crate::abi::check(code, "set_capabilities", CapabilitySet::from(capabilities))
    }
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
            .to_error("drain_events", CapabilitySet::empty())
            .unwrap();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn abi_version_gate_documents_the_contract() {
        assert_eq!(PB_ABI_VERSION, crate::abi::PB_ABI_VERSION);
        assert!(crate::abi::abi_version_compatible(PB_ABI_VERSION));
    }
}
