//! 目标进程内的软件断点（`0xCC` + 向量化异常处理）。
//!
//! ## 为什么是软件断点，不是硬件断点
//!
//! x86 的硬件断点只有 4 个（调试寄存器 `DR0..DR3`），而且要抢目标的调试寄存器——
//! 被分析的程序自己用（反调试）或别的工具占用时，我们没有公平的协调机制。
//! 软件断点没有数量上限，代价是要改一字节代码（改完立刻改回）。
//!
//! ## 命中后**自动解除**（一次性的）
//!
//! 传统调试器的做法是"命中 → 恢复原字节 → 单步执行一次 → 重新写入 `0xCC`"。
//! 那段舞蹈在**多线程目标**里是有竞态的：恢复原字节与重新写入之间，
//! 别的线程可能正好执行到这条指令——它看到的是一半被改过的代码。
//! 要安全地做这件事必须先把目标的所有线程停下来，而我们没有那个能力
//! （Pin 不是调试器，进程的线程由目标自己调度）。
//!
//! 所以这里选的是**明确的语义**而不是"看起来像调试器但会偶发崩目标"：
//!
//! * 命中一次后断点**自动解除**，事件里带上"已解除"标记；
//! * 想重复命中就重新调用 `pb_set_breakpoint`（脚本可以这么做，
//!   这正是脚本相对固定工具接口的优势：它自带状态）。
//!
//! 这条语义比"悄悄留下一个竞态"更可控，也更容易被 AI 正确使用——
//! 它会在事件里明确看到`已解除`，而不是在某个时刻收到一个莫名其妙的崩溃。
//!
//! ## 异常上下文只做最小记录
//!
//! 向量化异常处理器（VEH）跑在**出错的线程**上，上下文很敏感：它里面不能做
//! 分配、不能调外部 DLL、更不能长时间持锁。所以命中时只往一张**独立的**
//! 待处理表里追加一条记录，事件由正常上下文（下一次 `pb_drain_events` /
//! `pb_push_event`）取出并生成。
//!
//! 这样也避免了一个具体的自死锁：如果我们在持有 agent 全局状态锁的时候
//! 触及了断点页，处理器再去抢同一把锁就会自己等自己。

use pinbridge_core::error::{Error, Result};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 一条待上报的断点命中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakpointHit {
    /// 命中的地址（断点地址，不是异常返回地址）。
    pub address: u64,
    /// 命中它的线程。
    pub thread_id: u32,
    /// 命中瞬间的寄存器快照（Rip/Rsp/Rcx/Rdx/R8/R9）。
    ///
    /// 为什么必须在这里抓：命中的是**目标线程**，而 `pb_drain_events` 跑在
    /// agent 的链路线程上；等事件被取走时目标线程早就跑远了。
    /// 只报 (地址, 线程) 的断点在"谁调用了这个函数"这类问题上毫无用处——
    /// `[Rsp]` 才是返回地址。
    pub regs: [u64; 7],
    /// 命中现场从 `Rdx` 抄下来的原始字节（最多 64 字节）。
    ///
    /// 为什么必须在异常处理器里抄：调用方（例如协议序列化函数）把待写数据放在
    /// **栈上的临时变量**里，异常处理器一返回，那块栈立刻被下一条指令复用。
    /// 事后再从外面读只会拿到垃圾——本项目在逆 PoE 上行协议时真踩过这个坑。
    pub snap: [u8; 64],
    /// 有效快照长度（0 表示没抄）。
    pub snap_len: u32,
    /// 命中现场从**固定观测地址**抄下来的 32 字节。
    ///
    /// 为什么需要它：在 Flush 这种函数中间下断点时，Rcx 早就不是那个流对象了，
    /// 而"写缓冲在哪个地址"是稳定的——把它配成固定观测点，就能在**任意**断点处
    /// 看到那块内存此刻是明文还是密文，从而二分出"就地改写"发生在哪两条指令之间。
    pub snap2: [u8; 32],
    /// 有效长度（0 表示没配观测地址或读失败）。
    pub snap2_len: u32,
    /// 命中现场从 `R8` 指向的内存抄下来的 32 字节，长度取 `R9`。
    ///
    /// 为什么单列一份：x64 下很多接口是 `(this, x, ptr, len)`，第三个/第四个参数
    /// 分别在 R8/R9。抓密钥（`SetKey(this, nvp, key, 32)`）这类场景，
    /// `Rdx` 只是个标签指针，真正要的是 R8 —— 而栈上的临时密钥在异常处理器返回后
    /// 立刻会被复用，只能在这里抄。
    pub snap3: [u8; 32],
    /// 有效长度（0 表示 R9 不合理或读失败）。
    pub snap3_len: u32,
}

/// 固定观测地址（0 = 不启用）。由 `pb_set_snapshot` 设置。
static SNAP_ADDR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 设置固定观测地址。
pub fn set_snap_addr(addr: u64) {
    SNAP_ADDR.store(addr, std::sync::atomic::Ordering::Relaxed);
}

/// 读当前固定观测地址。
#[must_use]
pub fn snap_addr() -> u64 {
    SNAP_ADDR.load(std::sync::atomic::Ordering::Relaxed)
}

/// 断点表与待上报队列。
#[derive(Debug, Default)]
struct Table {
    /// 地址 → 被替换掉的原字节。
    armed: HashMap<u64, u8>,
    /// 待上报的命中（异常上下文只追加，正常上下文取走）。
    pending: Vec<BreakpointHit>,
}

fn table() -> &'static Mutex<Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(Table::default()))
}

/// 取出并清空待上报的命中。
pub fn take_pending() -> Vec<BreakpointHit> {
    let Ok(mut guard) = table().lock() else {
        return Vec::new();
    };
    std::mem::take(&mut guard.pending)
}

/// 当前已下的断点数量。
#[must_use]
pub fn armed_count() -> usize {
    table().lock().map_or(0, |guard| guard.armed.len())
}

/// 某个地址是否已经下了断点。
#[must_use]
pub fn is_armed(address: u64) -> bool {
    table()
        .lock()
        .is_ok_and(|guard| guard.armed.contains_key(&address))
}

/// 异常处理器是否已经装好。
fn handler_installed() -> &'static bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    INSTALLED.get_or_init(|| imp::install_handler().is_ok())
}

/// 在 `address` 上下一个断点。
///
/// # Errors
///
/// * 该地址已经下过断点（重复下会丢掉"原字节"的真相：第二次记下来的是 `0xCC`，
///   于是解除之后内存里留下一个永久的 `0xCC`）；
/// * 页面不可写且改保护失败；
/// * 异常处理器装不上。
pub fn arm(address: u64) -> Result<()> {
    if !handler_installed() {
        return Err(Error::TargetUnavailable(
            "装不上向量化异常处理器（AddVectoredExceptionHandler 失败）".into(),
        ));
    }
    if is_armed(address) {
        return Err(Error::TargetUnavailable(format!(
            "地址 0x{address:x} 已经有断点；重复下会丢掉原字节（解不干净），\
             先 pb_clear_breakpoint"
        )));
    }
    let original = imp::arm(address)?;
    let mut guard = table()
        .lock()
        .map_err(|_| Error::Script("断点表锁中毒".into()))?;
    guard.armed.insert(address, original);
    Ok(())
}

/// 解除 `address` 上的断点（把原字节写回去）。
///
/// # Errors
///
/// 该地址没有断点时返回错误——"解了一个不存在的断点"通常意味着调用方
/// 记错了地址，静默成功会让它一直以为那边有断点。
pub fn disarm(address: u64) -> Result<()> {
    let original = {
        let mut guard = table()
            .lock()
            .map_err(|_| Error::Script("断点表锁中毒".into()))?;
        guard
            .armed
            .remove(&address)
            .ok_or_else(|| Error::TargetUnavailable(format!("地址 0x{address:x} 上没有断点")))?
    };
    if let Err(error) = imp::disarm(address, original) {
        // 写回失败：把表项放回去，避免出现"表里没有、内存里还是 0xCC"的状态
        // ——那种状态下调用方既解不掉也重下不了。
        if let Ok(mut guard) = table().lock() {
            guard.armed.insert(address, original);
        }
        return Err(error);
    }
    Ok(())
}

/// 解除全部断点（进程收尾与测试清理用）。
pub fn disarm_all() {
    let addresses: Vec<(u64, u8)> = table().lock().map_or_else(
        |_| Vec::new(),
        |guard| guard.armed.iter().map(|(k, v)| (*k, *v)).collect(),
    );
    for (address, original) in addresses {
        let _ = imp::disarm(address, original);
    }
    if let Ok(mut guard) = table().lock() {
        guard.armed.clear();
    }
}

#[cfg(windows)]
mod imp {
    use super::{table, BreakpointHit};
    use pinbridge_core::error::{Error, Result};
    use std::ffi::c_void;

    /// 软件断点指令。
    const INT3: u8 = 0xCC;

    // EXCEPTION_BREAKPOINT
    const EXCEPTION_BREAKPOINT: u32 = 0x8000_0003;
    const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn ReadProcessMemory(
            process: *mut c_void,
            address: *const c_void,
            buffer: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> i32;
        fn WriteProcessMemory(
            process: *mut c_void,
            address: *mut c_void,
            buffer: *const c_void,
            size: usize,
            written: *mut usize,
        ) -> i32;
    }

    /// 读一个字节（**不裸解引用**）。
    ///
    /// 这条纪律是被实测逼出来的：把 `SetBreakpoint` 接到真实实现之后，
    /// 一条拿 `0x1000` 当"假地址"的老用例真的往 `0x1000` 写了 `0xCC`，
    /// 整个测试进程 `STATUS_ACCESS_VIOLATION`。**下断点的地址打错不能把目标打崩。**
    fn read_byte(address: u64) -> Result<u8> {
        let mut value: u8 = 0;
        let mut read: usize = 0;
        // SAFETY: value/read 指向本栈变量。
        let ok = unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                address as *const c_void,
                std::ptr::addr_of_mut!(value).cast::<c_void>(),
                1,
                &raw mut read,
            )
        };
        if ok == 0 || read != 1 {
            return Err(Error::TargetUnavailable(format!(
                "0x{address:x} 读不到（地址未映射或不可读），不能在这里下断点"
            )));
        }
        Ok(value)
    }

    /// 写一个字节。
    ///
    /// 用 `WriteProcessMemory` 而不是 `VirtualProtect` + 裸写：
    ///
    /// * **不改页的保护属性**——`WriteProcessMemory` 在单次调用内部完成必要的
    ///   处理，不会把目标页留在"被我改成可写"的状态里；
    /// * 在**异常处理器**里也安全：那里适合做系统调用，不适合改目标的内存属性。
    fn write_byte(address: u64, value: u8) -> Result<()> {
        let mut written: usize = 0;
        // SAFETY: value 指向本栈变量。
        let ok = unsafe {
            WriteProcessMemory(
                GetCurrentProcess(),
                address as *mut c_void,
                std::ptr::addr_of!(value).cast::<c_void>(),
                1,
                &raw mut written,
            )
        };
        if ok == 0 || written != 1 {
            return Err(Error::TargetUnavailable(format!(
                "0x{address:x} 写不进去（地址未映射或不可写）"
            )));
        }
        Ok(())
    }

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: *mut c_void,
        number_parameters: u32,
        _pad: u32,
        information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: Handler) -> *mut c_void;
    }

    type Handler = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

    /// 向量化异常处理器。
    ///
    /// # Safety
    ///
    /// 由系统在异常发生时调用；`pointers` 由系统保证有效。
    unsafe extern "system" fn on_exception(pointers: *mut ExceptionPointers) -> i32 {
        if pointers.is_null() {
            return 0;
        }
        // SAFETY: 系统传入的指针有效。
        let record = unsafe { (*pointers).record };
        if record.is_null() {
            return 0;
        }
        // SAFETY: record 非空。
        let exception = unsafe { &*record };
        if exception.code != EXCEPTION_BREAKPOINT {
            return 0; // 不是断点，交给别人
        }
        // 异常地址就是 `int3` 所在的位置（不是返回地址）。
        let address = exception.address as u64;
        let thread_id = current_thread_id();
        // 命中瞬间的寄存器：CONTEXT 由系统在异常里给出，指针有效。
        // 注意 Rip 指向 int3 **之后**那条指令；调用者的返回地址在 [Rsp]。
        let (regs, snap, snap_len, snap2, snap2_len, snap3, snap3_len) =
            read_regs((*pointers).context);
        // 命中即**自动解除**：把原字节写回去，并标记"先不要重装"。
        // 这里刻意不做"恢复 → 单步 → 重装"那段舞蹈，原因见模块文档。
        let Ok(mut guard) = table().lock() else {
            return EXCEPTION_CONTINUE_EXECUTION;
        };
        let Some(original) = guard.armed.remove(&address) else {
            // 不是我们下的断点（目标自己的 int3 或反调试）：交回去，
            // 让它走正常的异常处理路径——我们不该替目标决定它自己的 int3 怎么办。
            drop(guard);
            return 0;
        };
        guard.pending.push(BreakpointHit {
            address,
            thread_id,
            regs,
            snap,
            snap_len,
            snap2,
            snap2_len,
            snap3,
            snap3_len,
        });
        drop(guard);
        // 把原字节写回去。这一步在异常上下文里做**写内存**是必要的：
        // 不恢复的话目标会立刻在同一个地址上再次触发 int3，无限循环。
        if write_byte(address, original).is_err() {
            // 恢复失败：把表项塞回去（下次还能被解除），并让异常继续传播，
            // 让目标自己决定怎么办——比我们在这里死循环好。
            if let Ok(mut guard) = table().lock() {
                guard.armed.insert(address, original);
            }
            return 0;
        }
        EXCEPTION_CONTINUE_EXECUTION
    }

    /// 从 CONTEXT 里取 6 个寄存器（x64 固定偏移）。
    ///
    /// 为什么写死偏移而不是声明 `CONTEXT` 结构体：完整 `CONTEXT` 有 20 多个字段
    /// （含 XMM 保存区），少写一个字段就会让后面的字段整体错位——而偏移是
    /// Windows ABI 里**冻结**的。只读 6 个 u64，写错的可能性最小。
    ///
    /// # Safety
    ///
    /// `context` 必须是异常处理器拿到的那个 `CONTEXT*`（系统保证有效）。
    unsafe fn read_regs(
        context: *mut c_void,
    ) -> ([u64; 7], [u8; 64], u32, [u8; 32], u32, [u8; 32], u32) {
        if context.is_null() {
            return ([0; 7], [0; 64], 0, [0; 32], 0, [0; 32], 0);
        }
        // AMD64 CONTEXT 偏移：Rcx 0x80、Rdx 0x88、Rsp 0x98、R8 0xB8、R9 0xC0、Rip 0xF8
        const RSP: usize = 0x98;
        const RCX: usize = 0x80;
        const RDX: usize = 0x88;
        const R8: usize = 0xB8;
        const R9: usize = 0xC0;
        const RIP: usize = 0xF8;
        // SAFETY: 由调用方保证 context 有效；每个偏移都落在 CONTEXT 结构体内。
        unsafe {
            let base = context.cast::<u8>();
            // 快照里的偏移是任意的，**不保证 8 字节对齐** —— 必须用 read_unaligned，
            // 这不是"绕过 lint"：直接解引用未对齐指针在 x86 上能跑，但在别的架构上是 UB。
            let read = |off: usize| -> u64 { base.add(off).cast::<u64>().read_unaligned() };
            let rsp = read(RSP);
            // [Rsp] 就是调用者的返回地址。在异常上下文里读一次（和下面写回原字节同类操作）。
            let mut ret: u64 = 0;
            let mut got: usize = 0;
            let _ = ReadProcessMemory(
                GetCurrentProcess(),
                rsp as *const c_void,
                std::ptr::addr_of_mut!(ret).cast::<c_void>(),
                8,
                &raw mut got,
            );
            // 命中现场抄数据：x64 调用约定里 a1=Rcx, a2=Rdx, a3=R8。
            // 对 `Write(stream, src, len)` 来说 Rdx 就是调用方要写的那段数据，
            // 而它通常指向调用者的**栈临时量**——异常处理器一返回就会被复用，
            // 所以必须在这里抄走。
            let src = read(RDX);
            let want = read(R8);
            let mut snap = [0u8; 64];
            let mut snap_len: u32 = 0;
            if src != 0 && want > 0 && want <= 64 {
                let mut n: usize = 0;
                let _ = ReadProcessMemory(
                    GetCurrentProcess(),
                    src as *const c_void,
                    snap.as_mut_ptr().cast::<c_void>(),
                    want as usize,
                    &raw mut n,
                );
                if n == want as usize {
                    snap_len = want as u32;
                }
            }
            // 固定观测点：与寄存器无关，所以在函数的**任何**位置下断点都能看到这块内存。
            // 第三份快照：从 R8 指向的内存抄，长度取 R9（(this, x, ptr, len) 形态的接口）。
            let p3 = read(R8);
            let n3 = read(R9);
            let mut snap3 = [0u8; 32];
            let mut snap3_len: u32 = 0;
            if p3 != 0 && n3 > 0 && n3 <= 32 {
                let mut got3: usize = 0;
                let _ = ReadProcessMemory(
                    GetCurrentProcess(),
                    p3 as *const c_void,
                    snap3.as_mut_ptr().cast::<c_void>(),
                    n3 as usize,
                    &raw mut got3,
                );
                if got3 == n3 as usize {
                    snap3_len = n3 as u32;
                }
            }
            let watch = crate::breakpoint::SNAP_ADDR.load(std::sync::atomic::Ordering::Relaxed);
            let mut snap2 = [0u8; 32];
            let mut snap2_len: u32 = 0;
            if watch != 0 {
                let mut n2: usize = 0;
                let _ = ReadProcessMemory(
                    GetCurrentProcess(),
                    watch as *const c_void,
                    snap2.as_mut_ptr().cast::<c_void>(),
                    32,
                    &raw mut n2,
                );
                if n2 == 32 {
                    snap2_len = 32;
                }
            }
            (
                [
                    read(RIP),
                    rsp,
                    read(RCX),
                    read(RDX),
                    read(R8),
                    read(R9),
                    if got == 8 { ret } else { 0 },
                ],
                snap,
                snap_len,
                snap2,
                snap2_len,
                snap3,
                snap3_len,
            )
        }
    }

    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    fn current_thread_id() -> u32 {
        // SAFETY: 只取线程 id，无副作用。
        unsafe { GetCurrentThreadId() }
    }

    /// 读出原字节并写入 `0xCC`，返回原字节。
    pub(crate) fn arm(address: u64) -> Result<u8> {
        // 先证明这个地址**读得到**，再动手改：地址打错时得到的是一个错误，
        // 而不是目标进程的访问违例。
        let original = read_byte(address)?;
        write_byte(address, INT3)?;
        Ok(original)
    }

    /// 把原字节写回去。
    pub(crate) fn disarm(address: u64, original: u8) -> Result<()> {
        write_byte(address, original)
    }

    /// 装上向量化异常处理器（只装一次）。
    pub(crate) fn install_handler() -> Result<()> {
        // 参数 1：放在链首。我们要先看到断点异常，
        // 否则目标自己的处理器可能会把它们吞掉。
        // SAFETY: 处理器签名正确，系统会持有它。
        let handle = unsafe { AddVectoredExceptionHandler(1, on_exception) };
        if handle.is_null() {
            return Err(Error::TargetUnavailable(
                "AddVectoredExceptionHandler 返回空".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Result;
    use pinbridge_core::error::Error;

    /// # Errors
    ///
    /// 非 Windows 平台未实现。
    pub(crate) fn arm(_address: u64) -> Result<u8> {
        Err(Error::TargetUnavailable(
            "当前平台未实现软件断点（M1 只支持 Windows）".into(),
        ))
    }

    /// # Errors
    ///
    /// 非 Windows 平台未实现。
    pub(crate) fn disarm(_address: u64, _original: u8) -> Result<()> {
        Err(Error::TargetUnavailable(
            "当前平台未实现软件断点（M1 只支持 Windows）".into(),
        ))
    }

    /// # Errors
    ///
    /// 非 Windows 平台未实现。
    pub(crate) fn install_handler() -> Result<()> {
        Err(Error::TargetUnavailable(
            "当前平台未实现软件断点（M1 只支持 Windows）".into(),
        ))
    }
}

/// 当前平台是否实现了软件断点。
#[must_use]
pub const fn supported() -> bool {
    cfg!(windows)
}

/// 测试模块。
///
/// `pub` 是因为导出面的用例要把靶函数地址传进去（被测对象必须能被**真实地**
/// 下断点，用一个假的地址测不出任何东西）。
#[cfg(all(test, windows))]
pub mod tests {
    use super::*;

    /// 会被下断点的测试函数。
    ///
    /// `#[inline(never)]`：内联之后"这个函数"就不存在独立的代码地址了，
    /// 断点会下到调用方里去。
    ///
    /// `#[unsafe(no_mangle)]` 与 `pub`：导出面的用例要通过符号地址给它下断点。
    #[inline(never)]
    #[unsafe(no_mangle)]
    pub extern "C" fn pinbridge_breakpoint_target(value: u32) -> u32 {
        // 第一个字节就是断点位置。加一个可观察的副作用，用来验证
        // "命中之后函数仍然能正常执行完"。
        value.wrapping_mul(3).wrapping_add(1)
    }

    #[test]
    fn arm_fires_once_and_restores_the_original_byte() {
        // 单测之间串行：断点表是进程级共享状态。
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        disarm_all();
        let address = pinbridge_breakpoint_target as *const () as u64;
        println!("靶函数地址: 0x{address:x}");

        arm(address).expect("下断点应当成功");
        assert!(is_armed(address));
        assert_eq!(armed_count(), 1);

        // 重复下断点必须被拒绝：第二次记下来的"原字节"是 0xCC，
        // 解除之后会在内存里留下一个永久的 0xCC。
        let duplicate = arm(address).expect_err("重复下断点应当被拒");
        assert!(
            duplicate.to_string().contains("已经有断点"),
            "拒绝理由要说清楚：{duplicate}"
        );

        // 触发它。目标函数会先执行 `int3`，我们的处理器记录命中并恢复原字节。
        let result = pinbridge_breakpoint_target(7);
        assert_eq!(result, 22, "命中之后函数必须**仍然正常执行完**：{result}");

        let hits = take_pending();
        assert_eq!(hits.len(), 1, "应当恰好命中一次：{hits:?}");
        assert_eq!(hits[0].address, address);
        println!("✓ 命中一次：{hits:?}");

        // 命中即自动解除：表里没有了，内存里也没有 0xCC 了
        assert!(!is_armed(address), "命中后应当自动解除");
        assert_eq!(armed_count(), 0);
        // SAFETY: 读回那个字节，确认已经不是 int3
        let byte = unsafe { std::ptr::read_volatile(address as *const u8) };
        assert_ne!(byte, 0xCC, "原字节必须已经写回");
        // 再调一次：不会再有命中（已经解除了）
        assert_eq!(pinbridge_breakpoint_target(7), 22);
        assert!(take_pending().is_empty(), "解除之后不该再有命中");

        // 解除一个没下过的断点：必须报错（否则调用方会一直以为那边有断点）
        let missing = disarm(address).expect_err("解不存在的断点应当报错");
        assert!(missing.to_string().contains("没有断点"), "{missing}");
    }

    #[test]
    fn disarm_restores_before_any_hit() {
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        disarm_all();
        let address = pinbridge_breakpoint_target as *const () as u64;
        arm(address).expect("下断点应当成功");
        disarm(address).expect("解除应当成功");
        assert!(!is_armed(address));
        // SAFETY: 读回那个字节
        let byte = unsafe { std::ptr::read_volatile(address as *const u8) };
        assert_ne!(byte, 0xCC, "解除之后不能留下 int3");

        // 解除之后再调用：不该触发任何命中，且函数行为不变
        assert_eq!(pinbridge_breakpoint_target(3), 10);
        assert!(take_pending().is_empty());
    }

    #[test]
    fn self_made_int3_is_left_alone() {
        // 目标自己的 `int3`（反调试常见）不该被我们当成"自己的断点"处理：
        // 我们没有替目标决定它自己的异常该怎么办的权力。
        // 这里只验证表里没有它——真去执行目标自己的 int3 会把测试进程带崩，
        // 那正是"交回去让它走正常路径"的语义，不适合在单测里制造。
        let _guard = crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        disarm_all();
        assert!(!is_armed(0xDEAD_BEEF_0000));
        assert_eq!(armed_count(), 0);
    }
}
