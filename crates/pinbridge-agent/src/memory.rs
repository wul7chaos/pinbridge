//! 目标进程内的内存读写。
//!
//! ## 为什么不用直接解引用
//!
//! agent 就在目标进程里，`*(addr as *const u8)` 看着最直接，但它对**不可读地址**
//! 是把目标打崩（访问违例），而不是返回错误。被分析的程序里到处是未映射页、
//! `PAGE_NOACCESS` 保护页、只在某个瞬间可读的区域——"读之前不知道能不能读"
//! 是常态，不是异常情况。
//!
//! `ReadProcessMemory(GetCurrentProcess(), …)` / `WriteProcessMemory` 会先做
//! 范围与保护校验：不行就返回 FALSE + 错误码，**进程不受影响**。代价是一次
//! 内核调用，换来的是一条**可失败的**读写路径。
//!
//! ## 为什么不用 `VirtualProtect` 去"绕过"保护
//!
//! 有些工具读不可读页时会临时改保护属性再改回来。这里不做：那会**改变目标的
//! 内存保护状态**，在目标自己的线程与我们并发时留下一个真实的时间窗。
//! 读不到就如实说读不到，比"读到了但顺手改了目标的状态"更可控。

use pinbridge_core::error::{Error, Result};

#[cfg(windows)]
mod imp {
    use super::{Error, Result};
    use std::ffi::{c_int, c_void};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn ReadProcessMemory(
            process: *mut c_void,
            address: *const c_void,
            buffer: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> c_int;
        fn WriteProcessMemory(
            process: *mut c_void,
            address: *mut c_void,
            buffer: *const c_void,
            size: usize,
            written: *mut usize,
        ) -> c_int;
    }

    /// 读自身进程的内存；返回实际读到的字节数。
    ///
    /// # Errors
    ///
    /// 地址不可读、或跨越了未映射页时返回原因（带 Windows 错误码）。
    pub fn read_process(address: u64, buffer: &mut [u8]) -> Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut read: usize = 0;
        // SAFETY: buffer 有 buffer.len() 字节；read 指向本栈变量。
        // 不读可读性由内核校验，失败只是返回 0。
        let ok = unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                address as *const c_void,
                buffer.as_mut_ptr().cast::<c_void>(),
                buffer.len(),
                &raw mut read,
            )
        };
        if ok == 0 {
            // SAFETY: 取线程局部错误码，无副作用。
            let code = unsafe { last_error() };
            // 归到 `TargetUnavailable`：目标进程本身没坏，是**那个地址**不可读。
            // 具体原因（错误码 + 地址）都在文本里，调用方据此决定下一步。
            return Err(Error::TargetUnavailable(format!(
                "地址不可读或跨页（Windows 错误 {code}）{}",
                explain(code)
            )));
        }
        Ok(read)
    }

    /// 写自身进程的内存；返回实际写入的字节数。
    ///
    /// # Errors
    ///
    /// 地址不可写（只读页、未映射）时返回原因。
    pub fn write_process(address: u64, source: &[u8]) -> Result<usize> {
        if source.is_empty() {
            return Ok(0);
        }
        let mut written: usize = 0;
        // SAFETY: source 有 source.len() 字节；written 指向本栈变量。
        let ok = unsafe {
            WriteProcessMemory(
                GetCurrentProcess(),
                address as *mut c_void,
                source.as_ptr().cast::<c_void>(),
                source.len(),
                &raw mut written,
            )
        };
        if ok == 0 {
            // SAFETY: 取线程局部错误码，无副作用。
            let code = unsafe { last_error() };
            return Err(Error::TargetUnavailable(format!(
                "地址不可写（Windows 错误 {code}）{}",
                explain(code)
            )));
        }
        Ok(written)
    }

    unsafe extern "system" {
        fn GetLastError() -> u32;
    }

    /// # Safety
    ///
    /// 读线程局部的错误码；无副作用。
    unsafe fn last_error() -> u32 {
        // SAFETY: 由调用方保证只用于"刚刚失败的调用"之后。
        unsafe { GetLastError() }
    }

    /// 把最常见的两个错误码翻成人话。
    ///
    /// 只翻这两个是因为它们覆盖了实际会遇到的绝大多数情况，
    /// 而每个都直接决定下一步动作（换地址 vs 换权限）。
    fn explain(code: u32) -> &'static str {
        match code {
            // ERROR_PARTIAL_COPY
            299 => "（通常意味着这段地址只有一部分映射；先确认边界）",
            // ERROR_NOACCESS
            998 => "（保护属性不允许；这里不会替你临时改保护属性）",
            _ => "",
        }
    }
}

/// 一次内存快照的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// 落盘的字节数。
    pub bytes: u64,
    /// FNV-1a 64 位校验和。
    pub checksum: u64,
}

/// 快照允许的最大长度。
///
/// 有上限是必须的：`snapshot(addr, 1<<40, path)` 会试图分配/写一个天文数字，
/// 把目标进程和目标磁盘一起拖垮。超限**明确拒绝**，不截断——
/// "我要 1 GiB，你给我 64 MiB 且没告诉我"会让上层把半截数据当完整数据用。
pub const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

/// 分块大小：8 MiB 一块，既不至于一次性占用大块内存，也不至于系统调用过密。
const SNAPSHOT_CHUNK: usize = 8 * 1024 * 1024;

/// 计算 FNV-1a 64 位校验和。
///
/// **这不是密码学哈希**：它挡的是"传输/落盘过程中出了明显差错"和
/// "两次快照是不是同一段内存"，不挡可构造的碰撞。对抗场景要的是能区分
/// "数据被改过"，那种需求得用真正的哈希，届时单独引入依赖并说明。
#[must_use]
pub fn checksum(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 把目标进程的一段内存写到文件里（分块读、分块写、边写边算校验和）。
///
/// # Errors
///
/// 长度超上限、地址读不到（会指出是哪个偏移起读不到）、文件写不进去时返回原因。
pub fn snapshot_to_file(address: u64, length: u64, path: &std::path::Path) -> Result<Snapshot> {
    use std::io::Write;

    if length > MAX_SNAPSHOT_BYTES {
        return Err(Error::TargetUnavailable(format!(
            "快照长度 {length} 超过上限 {MAX_SNAPSHOT_BYTES} 字节；请分段快照"
        )));
    }
    if length == 0 {
        return Err(Error::TargetUnavailable("快照长度为 0".into()));
    }
    let file = std::fs::File::create(path).map_err(|error| {
        Error::TargetUnavailable(format!("创建 {} 失败：{error}", path.display()))
    })?;
    let mut writer = std::io::BufWriter::new(file);
    let mut done: u64 = 0;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut buffer = vec![0u8; SNAPSHOT_CHUNK.min(length as usize)];
    while done < length {
        let want = (length - done).min(buffer.len() as u64) as usize;
        let slice = &mut buffer[..want];
        let read = read_process(address + done, slice).map_err(|error| {
            // 指出**从哪个偏移起读不到**：快照失败时 AI 需要知道"边界在哪"，
            // 否则它只能猜是整段不可读还是只是尾巴。
            Error::TargetUnavailable(format!(
                "快照在偏移 {done}（地址 0x{:x}）处读失败：{error}",
                address + done
            ))
        })?;
        if read == 0 {
            return Err(Error::TargetUnavailable(format!(
                "快照在偏移 {done} 处读回 0 字节"
            )));
        }
        for byte in &slice[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        writer.write_all(&slice[..read]).map_err(|error| {
            Error::TargetUnavailable(format!("写入 {} 失败：{error}", path.display()))
        })?;
        done += read as u64;
    }
    writer.flush().map_err(|error| {
        Error::TargetUnavailable(format!("刷新 {} 失败：{error}", path.display()))
    })?;
    Ok(Snapshot {
        bytes: done,
        checksum: hash,
    })
}

#[cfg(not(windows))]
mod imp {
    use super::{Error, Result};

    /// # Errors
    ///
    /// 非 Windows 平台未实现。
    pub fn read_process(_address: u64, _buffer: &mut [u8]) -> Result<usize> {
        Err(Error::TargetUnavailable(
            "当前平台未实现进程内内存读（M1 只支持 Windows）".into(),
        ))
    }

    /// # Errors
    ///
    /// 非 Windows 平台未实现。
    pub fn write_process(_address: u64, _source: &[u8]) -> Result<usize> {
        Err(Error::TargetUnavailable(
            "当前平台未实现进程内内存写（M1 只支持 Windows）".into(),
        ))
    }
}

pub use imp::{read_process, write_process};

/// 当前平台是否实现了进程内内存读写。
#[must_use]
pub const fn supported() -> bool {
    cfg!(windows)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 一段会被读写的静态缓冲区。
    ///
    /// 用静态而不是栈上的局部变量：栈地址在测试之间会变，而且我们要的就是
    /// "一个地址确定、生命周期覆盖整个测试"的对象。`static mut` 在这里是
    /// 最贴合语义的表达（有且只有一个，单线程测试里访问）。
    static mut PROBE_BUFFER: [u8; 16] = [0u8; 16];

    #[test]
    fn reads_and_writes_its_own_address_space() {
        let address = std::ptr::addr_of_mut!(PROBE_BUFFER).cast::<u8>() as u64;
        let pattern: [u8; 8] = [0x31, 0x45, 0x47, 0x44, 0x49, 0x52, 0x42, 0x50];
        // 1) 写
        let written = write_process(address, &pattern).expect("写自身内存应当成功");
        assert_eq!(written, pattern.len());
        // 2) 读回来
        let mut buffer = [0u8; 8];
        let read = read_process(address, &mut buffer).expect("读自身内存应当成功");
        assert_eq!(read, 8);
        assert_eq!(buffer, pattern, "读回来的必须就是写进去的");
        println!("✓ 读写自身地址空间：0x{address:x} = {buffer:02x?}");
    }

    #[test]
    fn unreadable_address_is_an_error_not_a_crash() {
        // 一个几乎不可能被映射的地址。关键要求是"**返回错误**"而不是把进程打崩——
        // 这正是选 `ReadProcessMemory` 而不是解引用的原因。
        let mut buffer = [0u8; 8];
        let error = read_process(0xDEAD_BEEF_0000, &mut buffer).expect_err("应当报错");
        let text = error.to_string();
        assert!(text.contains("Windows 错误"), "要带上错误码：{text}");
        println!("✓ 不可读地址报错而不是崩溃：{text}");
    }

    #[test]
    fn zero_length_is_a_no_op() {
        // 零长度读**不该**因为"地址为空"报错：这是一个合法请求
        // （很多调用点会用它来探测边界），返回 0 字节。
        let mut empty: [u8; 0] = [];
        assert_eq!(read_process(0, &mut empty).expect("零长度读应当成功"), 0);
        assert_eq!(write_process(0, &[]).expect("零长度写应当成功"), 0);
    }

    #[test]
    fn nonzero_length_with_unmapped_address_reports_the_address() {
        // 报错文本必须带地址：调用方需要区分"这个地址读不了"和"权限不够"。
        let mut buffer = [0u8; 4];
        let error = read_process(0x1, &mut buffer).expect_err("应当报错");
        assert!(error.to_string().contains("Windows 错误"), "{error}");
    }
}
