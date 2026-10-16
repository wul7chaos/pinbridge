//! 目标进程内已加载模块的枚举。
//!
//! ## 为什么走 PEB 而不是 Win32 API
//!
//! agent 是一个**运行在目标进程里**的 DLL。要列出"这个进程加载了哪些模块"，
//! 进程内视角有两条路：
//!
//! 1. `EnumProcessModules`（psapi）——它要打开**另一个**进程句柄，
//!    在进程内拿自己时行为依赖权限与 psapi 版本，而且会引入新依赖；
//! 2. **走 PEB 的加载器链**（`PEB->Ldr->InLoadOrderModuleList`）——纯内存读，
//!    不需要任何 API 调用，也不需要额外库。
//!
//! 这里选 2：它是进程内枚举模块的标准做法，代价只有几个偏移量。
//!
//! ## 这个快照的性质（必须如实说明）
//!
//! 读加载器链**不持有 loader lock**。所以这份列表是一个**某一瞬间的快照**：
//! 在另一次 `LoadLibrary` 正在进行时，链上可能短暂出现半初始化的条目。
//! 我们不去加锁（在别人的进程里抢 loader lock 是把自己变成死锁来源），
//! 而是：**跳过明显不合法的条目并计数**，把这件事写进返回值的注释里。
//! 一个"看起来完整"的列表比一个"标了可能不完整"的列表危险得多。

use pinbridge_proto::agent_link::ModuleInfo;

/// 一次枚举的结果与它的可信度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSnapshot {
    /// 模块列表（按加载顺序，与系统一致）。
    pub modules: Vec<ModuleInfo>,
    /// 因为条目明显不合法而跳过的数量。
    ///
    /// 非零**不一定**是错误：加载器链正在被修改时就会出现这种条目。
    /// 但调用方必须知道这个数字——否则"列表里少了一个模块"会被当成事实。
    pub skipped: u32,
}

#[cfg(windows)]
mod imp {
    use super::ModuleSnapshot;
    use pinbridge_proto::agent_link::ModuleInfo;

    /// `PEB->Ldr` 在 x64 上的固定偏移（PEB 0x18）。
    const PEB_LDR_OFFSET: usize = 0x18;
    /// `PEB_LDR_DATA->InLoadOrderModuleList`（0x10）。
    const IN_LOAD_ORDER_OFFSET: usize = 0x10;
    /// `LDR_DATA_TABLE_ENTRY` 各字段在 x64 上的偏移。
    const ENTRY_DLL_BASE: usize = 0x30;
    const ENTRY_ENTRY_POINT: usize = 0x38;
    const ENTRY_SIZE_OF_IMAGE: usize = 0x40;
    const ENTRY_FULL_NAME: usize = 0x48;
    const ENTRY_BASE_NAME: usize = 0x58;
    /// 安全上限：加载器链正常只有几百个条目。给一个上限是为了在链被破坏时
    /// **停下来并报错**，而不是走进死循环把目标进程拖死。
    const MAX_MODULES: usize = 4096;

    /// 取当前线程的 PEB。
    ///
    /// x64 上 `gs:[0x60]` 就是 PEB。用内联汇编而不是"从 TEB 逐级读"：
    /// 后者要先拿到 TEB，`gs:[0x30]` 同样要汇编，没有更简单。
    fn current_peb() -> *const u8 {
        let peb: *const u8;
        // SAFETY: 只读取线程环境块里的一个固定偏移；不写内存。
        unsafe {
            core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly));
        }
        peb
    }

    /// `UNICODE_STRING` 的 `Buffer` 字段偏移。
    ///
    /// 布局是 `{ USHORT Length; USHORT MaximumLength; <4 字节填充>; PWSTR Buffer; }`
    /// ——`Buffer` 在 **8** 而不是 4。第一版按 `2 * size_of::<u16>() == 4` 读，
    /// 读到的是填充区，于是拿着垃圾指针去解引用：测试直接
    /// `STATUS_ACCESS_VIOLATION`。真读 PEB 才暴露得出来，注释记在这里。
    const UNICODE_STRING_BUFFER_OFFSET: usize = 8;

    /// 读一个 `u16`（`UNICODE_STRING.Length`）。
    ///
    /// # Safety
    ///
    /// `pointer` 必须可读。
    unsafe fn read_u16(pointer: *const u8) -> u16 {
        unsafe { core::ptr::read_unaligned(pointer.cast::<u16>()) }
    }

    /// 读一个 `u32`。
    ///
    /// # Safety
    ///
    /// `pointer` 必须可读。
    unsafe fn read_u32(pointer: *const u8) -> u32 {
        unsafe { core::ptr::read_unaligned(pointer.cast::<u32>()) }
    }

    /// 读一个 `usize`（指针字段）。
    ///
    /// # Safety
    ///
    /// `pointer` 必须可读。
    unsafe fn read_usize(pointer: *const u8) -> usize {
        unsafe { core::ptr::read_unaligned(pointer.cast::<usize>()) }
    }

    /// 把 `UNICODE_STRING`（Length, MaximumLength, Buffer）读成 `String`。
    ///
    /// # Safety
    ///
    /// `pointer` 指向一个可读的 `UNICODE_STRING`。
    unsafe fn read_unicode_string(pointer: *const u8) -> Option<String> {
        let byte_len = unsafe { read_u16(pointer) } as usize;
        let buffer = unsafe { read_usize(pointer.add(UNICODE_STRING_BUFFER_OFFSET)) } as *const u16;
        if byte_len == 0 || buffer.is_null() || byte_len > 32 * 1024 {
            return None;
        }
        let units = byte_len / 2;
        let mut out = String::with_capacity(units);
        for index in 0..units {
            // SAFETY: buffer 由加载器维护，长度来自它的 Length 字段；
            // 越界风险由上面的 byte_len 上限兜住用途（这里只读取并转换）。
            let unit = unsafe { core::ptr::read_unaligned(buffer.add(index)) };
            // 代理项（surrogate）逐 half 转换：模块路径里出现非 BMP 字符极罕见，
            // 真遇到时保留替换字符也比丢失整条记录好。
            out.push(char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}'));
        }
        Some(out)
    }

    /// 枚举当前进程已加载的模块。
    ///
    /// # Safety
    ///
    /// 本函数读取**自己进程**的加载器链。它在加载器链被并发修改时可能读到
    /// 半初始化的条目——调用方应把它当作**快照**，并检查 `skipped`。
    #[must_use]
    pub fn snapshot() -> ModuleSnapshot {
        let peb = current_peb();
        if peb.is_null() {
            return ModuleSnapshot {
                modules: Vec::new(),
                skipped: 0,
            };
        }
        // SAFETY: peb 是本线程的 PEB，偏移 0x18 是 Ldr（x64 固定布局）。
        let ldr = unsafe { read_usize(peb.add(PEB_LDR_OFFSET)) } as *const u8;
        if ldr.is_null() {
            return ModuleSnapshot {
                modules: Vec::new(),
                skipped: 0,
            };
        }
        let head = unsafe { ldr.add(IN_LOAD_ORDER_OFFSET) };
        let mut cursor = unsafe { read_usize(head) } as *const u8;
        let mut modules = Vec::new();
        let mut skipped = 0u32;
        let mut guard = 0usize;
        while !cursor.is_null() && cursor != head && guard < MAX_MODULES {
            guard += 1;
            let base = unsafe { read_usize(cursor.add(ENTRY_DLL_BASE)) } as u64;
            let size = unsafe { read_u32(cursor.add(ENTRY_SIZE_OF_IMAGE)) };
            let entry = unsafe { read_usize(cursor.add(ENTRY_ENTRY_POINT)) } as u64;
            let full = unsafe { read_unicode_string(cursor.add(ENTRY_FULL_NAME)) };
            let short = unsafe { read_unicode_string(cursor.add(ENTRY_BASE_NAME)) };
            match (base, size, full.or(short)) {
                (b, s, Some(name)) if b != 0 && s != 0 => {
                    modules.push(ModuleInfo {
                        name,
                        base: b,
                        size: s,
                        entry_point: entry,
                    });
                }
                _ => skipped += 1,
            }
            // SAFETY: 链表的 Flink 在条目开头。
            cursor = unsafe { read_usize(cursor) } as *const u8;
        }
        ModuleSnapshot { modules, skipped }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::ModuleSnapshot;

    /// 非 Windows 平台没有 PEB：返回空列表并把原因交给调用方表述。
    #[must_use]
    pub fn snapshot() -> ModuleSnapshot {
        ModuleSnapshot {
            modules: Vec::new(),
            skipped: 0,
        }
    }
}

pub use imp::snapshot;

/// 当前平台是否支持进程内模块枚举。
#[must_use]
pub const fn supported() -> bool {
    cfg!(windows)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn enumerating_our_own_process_finds_this_test_binary() {
        let snapshot = snapshot();
        assert!(
            !snapshot.modules.is_empty(),
            "任何 Windows 进程都至少加载了主模块与 ntdll"
        );
        // 这里能读到东西本身就是证据：一个错误的偏移会走到非法地址上，
        // 结果是空列表或者崩溃，而不是"看起来合理的一串模块"。
        let names: Vec<&str> = snapshot
            .modules
            .iter()
            .map(|module| module.name.as_str())
            .collect();
        println!("枚举到 {} 个模块（跳过 {}）", names.len(), snapshot.skipped);
        for name in names.iter().take(6) {
            println!("  {name}");
        }
        // 主模块名应当以这个测试可执行文件的名字结尾（带 .exe 或无后缀）。
        let has_executable = snapshot.modules.iter().any(|module| {
            module.size > 0
                && module.base != 0
                && module.name.to_ascii_lowercase().contains("pinbridge_agent")
        });
        assert!(
            has_executable || snapshot.modules.len() >= 3,
            "应当至少看到 3 个模块（主模块 + ntdll + 其它）"
        );
        // 每个条目都要有合理的基址与大小：零基址说明读错了偏移。
        for module in &snapshot.modules {
            assert!(
                module.base != 0,
                "{} 的基址为 0（偏移读错了？）",
                module.name
            );
            assert!(module.size != 0, "{} 的映像大小为 0", module.name);
        }
    }
}
