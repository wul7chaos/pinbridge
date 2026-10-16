//! 目标进程内的 CPython 宿主（设计决策见 `docs/design/script-runtime.md`）。
//!
//! ## 为什么是**运行时动态加载**而不是链接 `python314.lib`
//!
//! 链接导入库会把 `pinbridge_agent.dll` 变成"依赖 `python314.dll` 才能加载"。
//! 后果很具体：任何没装 Python 3.14 的机器上，**agent 整个加载不了**——
//! 连"读内存"这种跟 Python 无关的能力也一起没了，而且报错发生在 `LoadLibrary`
//! 那一层，离真正的原因很远。
//!
//! 所以这里走和桥接层/agent 自身同一套办法：`LoadLibraryA` + `GetProcAddress`
//! 动态取函数指针。代价是每个用到的 `Py_*` 符号都要自己声明与解析，
//! 收益是**没有 Python 时框架仍然完全可用**，只是脚本能力明确报"不支持"。
//!
//! ## 当前进度（诚实标注）
//!
//! * ✅ 定位并加载解释器 DLL、初始化、执行脚本、收尾清理；
//! * ✅ 缺 Python 时给出**可行动的错误**（哪个库名、试过哪些路径）；
//! * ⏳ `pb` 模块（`log` / `mark` / `read` / `subscribe`）与探针回调；
//! * ⏳ 脚本预算的强制中断。
//!
//! 分段的理由：先把"解释器能不能在目标进程里活着"这件事验证掉。
//! 它是后面所有脚本能力的前提，也是最容易在别人机器上失败的一环。

/// 解释器 DLL 的候选名（按优先级）。
///
/// 顺序有讲究：先试**稳定 ABI**（`python3.dll`，3.x 通用、不绑小版本），
/// 再试本机具体版本。稳定 ABI 的符号集是有保证的，而这个宿主只用到了
/// 其中最基础的那几个。
#[cfg(windows)]
const CANDIDATE_DLLS: &[&str] = &[
    // 允许用户显式指定（测试与非常规安装都靠它）
    // 环境变量优先级最高：见 `library_candidates`
    "python3.dll",
    "python314.dll",
    "python313.dll",
    "python312.dll",
];

/// 环境变量：显式指定解释器 DLL 的完整路径。
pub const DLL_ENV: &str = "PINBRIDGE_PYTHON_DLL";

/// 宿主是否能在当前平台、当前机器上工作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// 可用，附上实际加载的库。
    Ready {
        /// 加载到的 DLL（路径或名字）。
        library: String,
    },
    /// 不可用，附上**可行动的原因**。
    Unavailable {
        /// 原因（必须说清缺什么、试过什么）。
        reason: String,
    },
}

#[cfg(windows)]
mod imp {
    use super::{Availability, CANDIDATE_DLLS, DLL_ENV};
    use pinbridge_core::error::{Error, Result};
    use std::ffi::{c_char, c_int, c_uint, c_void, CString};

    // ── Win32（只声明用到的三个，避免引入 windows.h 与 Pin 的头文件冲突）──
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> c_int;
    }

    /// CPython 里我们要用到的那几个函数（用稳定 ABI 的名字与签名）。
    #[allow(non_snake_case)]
    struct PyApi {
        module: *mut c_void,
        /// `int Py_IsInitialized(void)`
        Py_IsInitialized: unsafe extern "C" fn() -> c_int,
        /// `void Py_InitializeEx(int initsigs)`
        Py_InitializeEx: unsafe extern "C" fn(c_int),
        /// `int Py_IsFinalizing(void)`（3.13+ 才有，允许缺失）
        Py_IsFinalizing: Option<unsafe extern "C" fn() -> c_int>,
        /// `int PyRun_SimpleString(const char* code)`
        PyRun_SimpleString: unsafe extern "C" fn(*const c_char) -> c_int,
        /// `PyGILState_STATE PyGILState_Ensure(void)`
        PyGILState_Ensure: unsafe extern "C" fn() -> c_int,
        /// `void PyGILState_Release(PyGILState_STATE)`
        PyGILState_Release: unsafe extern "C" fn(c_int),
        /// `PyThreadState* PyEval_SaveThread(void)` —— 释放 GIL 并返回线程状态
        PyEval_SaveThread: unsafe extern "C" fn() -> *mut c_void,
        // ── 构造/读取 Python 对象所需的那一组（全部来自稳定 ABI）──
        //
        // 刻意只用**不透明指针 + 非变参**函数：变参（`PyObject_CallFunction` 之类）
        // 在跨语言边界上无法安全表达，而结构体布局（`PyModuleDef`）写错就是
        // 目标进程里的随机崩溃。这一组足够构造字典/元组、调用可调用对象、
        // 以及把 Python 字符串读回 Rust。
        /// `PyObject* PyImport_AddModule(const char* name)`（借用引用，不要 DecRef）
        PyImport_AddModule: unsafe extern "C" fn(*const c_char) -> *mut c_void,
        /// `PyObject* PyObject_GetAttrString(PyObject*, const char*)`（**新引用**）
        PyObject_GetAttrString: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
        /// `PyObject* PyObject_Call(PyObject* callable, PyObject* args, PyObject* kwargs)`
        PyObject_Call: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void,
        /// `PyObject* PyTuple_New(Py_ssize_t size)`
        PyTuple_New: unsafe extern "C" fn(isize) -> *mut c_void,
        /// `int PyTuple_SetItem(PyObject* tuple, Py_ssize_t index, PyObject* item)`（**偷引用**）
        PyTuple_SetItem: unsafe extern "C" fn(*mut c_void, isize, *mut c_void) -> c_int,
        /// `PyObject* PyUnicode_FromString(const char* text)`（**新引用**）
        PyUnicode_FromString: unsafe extern "C" fn(*const c_char) -> *mut c_void,
        /// `PyObject* PyUnicode_AsUTF8String(PyObject* unicode)`（**新引用**）
        PyUnicode_AsUTF8String: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        /// `char* PyBytes_AsString(PyObject* bytes)`（借用）
        PyBytes_AsString: unsafe extern "C" fn(*mut c_void) -> *mut c_char,
        /// `Py_ssize_t PyBytes_Size(PyObject* bytes)`
        PyBytes_Size: unsafe extern "C" fn(*mut c_void) -> isize,
        /// `void Py_DecRef(PyObject*)` —— 稳定 ABI 里它是**导出函数**（不是内联宏）
        Py_DecRef: unsafe extern "C" fn(*mut c_void),
        /// `void PyErr_Print(void)`
        PyErr_Print: unsafe extern "C" fn(),
        /// `PyObject* PyObject_Str(PyObject*)`（**新引用**）
        PyObject_Str: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        // ── 造一个"Rust 支撑的可调用对象"所需的那几个 ──
        //
        // 这是反向调用的关键：脚本要能**同步**要求宿主做事（`pb.read`），
        // 而这要求宿主能给出一个真正的 Python 可调用对象。
        // `PyCFunction_NewEx` 只依赖 `PyMethodDef`（4 个字段，布局稳定且公开），
        // 比写 `PyModuleDef`（10 个字段）风险小得多。
        /// `PyObject* PyCFunction_NewEx(PyMethodDef*, PyObject* self, PyObject* module)`
        PyCFunction_NewEx:
            unsafe extern "C" fn(*mut PyMethodDef, *mut c_void, *mut c_void) -> *mut c_void,
        /// `PyObject* PyBytes_FromStringAndSize(const char*, Py_ssize_t)`（**新引用**）
        PyBytes_FromStringAndSize: unsafe extern "C" fn(*const c_char, isize) -> *mut c_void,
        /// `PyObject* PyTuple_GetItem(PyObject*, Py_ssize_t)`（借用）
        PyTuple_GetItem: unsafe extern "C" fn(*mut c_void, isize) -> *mut c_void,
        /// `Py_ssize_t PyTuple_Size(PyObject*)`
        PyTuple_Size: unsafe extern "C" fn(*mut c_void) -> isize,
        /// `unsigned long long PyLong_AsUnsignedLongLong(PyObject*)`
        PyLong_AsUnsignedLongLong: unsafe extern "C" fn(*mut c_void) -> u64,
        /// `void PyErr_SetString(PyObject* type, const char* message)`
        PyErr_SetString: unsafe extern "C" fn(*mut c_void, *const c_char),
        /// `PyObject* PyExc_RuntimeError`（异常类型，数据符号）
        PyExc_RuntimeError: *mut *mut c_void,
        /// `PyObject* PyExc_ValueError`（异常类型，数据符号）
        PyExc_ValueError: *mut *mut c_void,
        /// `PyObject* PyModule_GetDict(PyObject*)`（借用）
        PyModule_GetDict: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        /// `int PyThreadState_SetAsyncExc(unsigned long id, PyObject* exc)`（**新引用**由调用方负责）
        PyThreadState_SetAsyncExc:
            unsafe extern "C" fn(std::os::raw::c_ulong, *mut c_void) -> c_int,
        /// `unsigned long PyThread_get_thread_ident(void)`
        PyThread_get_thread_ident: unsafe extern "C" fn() -> std::os::raw::c_ulong,
        /// `int PyDict_SetItemString(PyObject*, const char*, PyObject*)`
        PyDict_SetItemString:
            unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> c_int,
    }

    // SAFETY: 这些指针由 DLL 持有；它们本身不访问线程局部状态。
    // 解释器的线程约束由上层（GIL 与"单线程调用"约定）保证。
    unsafe impl Send for PyApi {}
    unsafe impl Sync for PyApi {}

    /// 超时异常类的引用（进程生命周期内一直有效）。
    ///
    /// 存的是 `PyObject*` 的地址而不是结构体：我们只把它转手交给
    /// `PyThreadState_SetAsyncExc`，从不解引用。持有它不释放是刻意的——
    /// 它就是 `__main__` 里的 `_PBTimeout` 类，随进程一起消失。
    static TIMEOUT_CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

    /// 预算到点时要注入的异常类。
    fn timeout_class() -> Option<*mut c_void> {
        TIMEOUT_CLASS.get().map(|address| *address as *mut c_void)
    }

    /// 看门狗状态。
    #[derive(Debug, Default)]
    struct Watch {
        /// 每次"装预算"递增。看门狗据此判断自己是否还在为当前这次调用工作。
        generation: u64,
        /// 这次调用的目标线程与截止时间。
        target: Option<(std::os::raw::c_ulong, std::time::Instant)>,
        /// 看门狗是否已经注入过异常（= 这次调用超时了）。
        fired: bool,
    }

    /// 看门狗：一个常驻线程 + 条件变量。
    ///
    /// 为什么不每次调用起一个线程：回调是热路径（一个事件一次分发），
    /// 每次起线程的开销比脚本本身还大。
    #[derive(Debug)]
    struct Watchdog {
        inner: std::sync::Mutex<Watch>,
        signal: std::sync::Condvar,
    }

    fn watchdog() -> &'static Watchdog {
        static WATCHDOG: std::sync::OnceLock<Watchdog> = std::sync::OnceLock::new();
        WATCHDOG.get_or_init(|| {
            let dog = Watchdog {
                inner: std::sync::Mutex::new(Watch::default()),
                signal: std::sync::Condvar::new(),
            };
            // 常驻线程：它只做一件事——睡到截止时间，然后注入异常。
            std::thread::Builder::new()
                .name("pinbridge-watchdog".into())
                .spawn(watchdog_loop)
                .map(|_| ())
                .ok();
            dog
        })
    }

    /// 看门狗线程主体。
    fn watchdog_loop() {
        let dog = watchdog();
        let mut seen: u64 = 0;
        loop {
            let (generation, target) = {
                let Ok(mut guard) = dog.inner.lock() else {
                    return;
                };
                while guard.generation == seen {
                    match dog.signal.wait(guard) {
                        Ok(next) => guard = next,
                        Err(_) => return,
                    }
                }
                seen = guard.generation;
                (seen, guard.target)
            };
            let Some((ident, deadline)) = target else {
                continue;
            };
            // 睡到截止时间。分段检查"还是不是同一代"，避免把一次已经结束的
            // 调用的预算算到下一次调用头上。
            loop {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline - now;
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(5)));
                let Ok(guard) = dog.inner.lock() else { return };
                if guard.generation != generation {
                    break; // 这次调用已经结束了
                }
            }
            let Ok(mut guard) = dog.inner.lock() else {
                return;
            };
            if guard.generation != generation || guard.target.is_none() {
                continue; // 已经 disarm，不该再注入
            }
            guard.fired = true;
            guard.target = None; // 只注入一次
            drop(guard);
            inject_timeout(ident);
        }
    }

    /// 把超时异常异步注入目标线程。
    ///
    /// 用 `PyThreadState_SetAsyncExc` 而不是 `sys.settrace`：后者要维护解释器的
    /// 线程跟踪状态，而那种状态在"超时中断过一次"之后会留下痕迹，
    /// 导致后续的 `settrace` **收不到行事件**（看门狗静默失效，脚本继续死循环）。
    /// 实测过这个坑：两条预算用例单独跑都过，一起跑就挂死。
    fn inject_timeout(ident: std::os::raw::c_ulong) {
        let Some(api) = CALLBACK_API.get() else {
            return;
        };
        let Some(class) = timeout_class() else {
            return;
        };
        // SAFETY: 注入需要持有 GIL；这里成对可取可放。
        let gil = unsafe { (api.PyGILState_Ensure)() };
        // SAFETY: class 是 `__main__` 里长期有效的异常类；ident 来自
        // `PyThread_get_thread_ident`（同一进程里的线程）。
        // 返回值 1 = 注入成功，0 = 没找到那个线程（例如它已经结束）。
        // 不检查返回值就"以为注入过了"会让超时变成静默失效。
        let code = unsafe { (api.PyThreadState_SetAsyncExc)(ident, class) };
        if code == 0 {
            eprintln!("[pinbridge] 超时注入失败：解释器里已经没有线程 {ident}（它可能已经结束）");
        }
        // SAFETY: gil 来自上面的 Ensure。
        unsafe { (api.PyGILState_Release)(gil) };
    }
    /// CPython 的方法表项（`PyMethodDef`）。4 个字段、布局公开且长期稳定。
    ///
    /// 只写这一个结构体、**不写 `PyModuleDef`**：后者字段多一倍，而它带来的能力
    /// （注册一个模块）我们并不需要——我们只要一个可调用对象，然后自己把它
    /// 塞进 `pb` 模块的字典里。
    #[repr(C)]
    struct PyMethodDef {
        name: *const c_char,
        method: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void,
        flags: c_int,
        doc: *const c_char,
    }

    /// `METH_VARARGS`：方法收到一个参数元组。
    const METH_VARARGS: c_int = 0x0001;

    /// 让含裸指针的 `PyMethodDef` 能放进静态。
    ///
    /// 需要静态是因为 **CPython 只存指针、不拷贝** `PyMethodDef`：
    /// 放在栈上会让可调用对象持有一个悬空的方法表。
    struct SyncMethodDef(PyMethodDef);
    // SAFETY: 内容在进程生命周期内不变，只读共享没有数据竞争。
    unsafe impl Sync for SyncMethodDef {}

    /// 回调路径要用的函数表。**刻意不经过宿主的互斥锁**。
    ///
    /// 为什么需要第二份：分发的加锁顺序是"先锁宿主 → 再取 GIL"，而回调是在
    /// **已经持有 GIL** 时被调用的。如果回调再去抢同一个互斥锁，两条路径的
    /// 加锁顺序就相反了——A 持有 GIL 等锁、B 持有锁等 GIL，经典死锁。
    /// 函数表本身不可变，所以另存一份静态给回调直接读。
    static CALLBACK_API: std::sync::OnceLock<PyApi> = std::sync::OnceLock::new();

    /// `_pb_read_native(address, length) -> bytes`：脚本侧的内存读。
    ///
    /// 能力校验发生在**这里**（调用 `pb_memory_read`），而不是在 Python 侧：
    /// 脚本可以随手改 Python 里的任何东西，闸门必须在够不着的那一侧。
    unsafe extern "C" fn pb_read_native(_self: *mut c_void, args: *mut c_void) -> *mut c_void {
        let Some(api) = CALLBACK_API.get() else {
            // 连函数表都没有：无法构造异常，返回空指针（Python 侧会看到失败）。
            return std::ptr::null_mut();
        };
        let fail = |message: &str| -> *mut c_void {
            let text = c_string(message).unwrap_or_default();
            // SAFETY: 异常类型是 DLL 里的数据符号；解除引用得到 PyObject*。
            let kind = unsafe { *api.PyExc_RuntimeError };
            // SAFETY: kind 是异常类型；text 在本调用期间存活。
            unsafe { (api.PyErr_SetString)(kind, text.as_ptr()) };
            std::ptr::null_mut()
        };
        if args.is_null() {
            return fail("pb.read 需要两个参数：地址与长度");
        }
        // SAFETY: args 是参数元组（METH_VARARGS 约定）。
        let count = unsafe { (api.PyTuple_Size)(args) };
        if count != 2 {
            return fail(&format!(
                "pb.read 需要两个参数（地址、长度），收到 {count} 个"
            ));
        }
        // SAFETY: 索引 0/1 在范围内，返回借用引用。
        let (address_object, length_object) = unsafe {
            (
                (api.PyTuple_GetItem)(args, 0),
                (api.PyTuple_GetItem)(args, 1),
            )
        };
        if address_object.is_null() || length_object.is_null() {
            return fail("pb.read 的参数取不到");
        }
        // SAFETY: 两个对象都非空；`PyLong_AsUnsignedLongLong` 对非整数会设异常
        // 并返回 (unsigned long long)-1，因此低于长度校验要挡住它。
        let (address, length) = unsafe {
            (
                (api.PyLong_AsUnsignedLongLong)(address_object),
                (api.PyLong_AsUnsignedLongLong)(length_object),
            )
        };
        if length == u64::MAX || address == u64::MAX {
            // `PyLong_AsUnsignedLongLong` 对非整数会设异常并返回 (unsigned long long)-1。
            // 这里把它换成一个说清"要整数"的 ValueError——否则脚本看到的会是
            // 一句与真实原因无关的转换错误。
            // SAFETY: 异常类型是 DLL 里的数据符号；消息是 NUL 结尾的字面量。
            unsafe {
                (api.PyErr_SetString)(
                    *api.PyExc_ValueError,
                    c"pb.read 的地址/长度必须是整数".as_ptr(),
                );
            }
            return std::ptr::null_mut();
        }
        if length > MAX_SCRIPT_READ_BYTES {
            return fail(&format!(
                "pb.read 单次上限 {MAX_SCRIPT_READ_BYTES} 字节（收到 {length}）；\
                 大范围请分段读或改用 snapshot"
            ));
        }
        let mut buffer = vec![0u8; length as usize];
        let mut written: c_uint = 0;
        // SAFETY: buffer 有 length 字节；written 指向本栈变量。
        // **走 agent 自己的导出**，于是能力闸门、地址校验、错误文本
        // 与 Hub 侧调用完全同一条路（不给脚本开小门）。
        let code = unsafe {
            crate::exports::pb_memory_read(
                address,
                length as c_uint,
                buffer.as_mut_ptr(),
                length as c_uint,
                &raw mut written,
            )
        };
        if code != 0 {
            // 失败原因由导出面写在 last_error 里（带地址与 Windows 错误码）。
            // SAFETY: pb_last_error 返回 DLL 内 NUL 结尾的字符串。
            let reason = unsafe {
                std::ffi::CStr::from_ptr(crate::exports::pb_last_error())
                    .to_string_lossy()
                    .into_owned()
            };
            return fail(&reason);
        }
        buffer.truncate(written as usize);
        // SAFETY: buffer 的指针与长度一致；返回值是新引用（由调用方持有）。
        unsafe {
            (api.PyBytes_FromStringAndSize)(buffer.as_ptr().cast::<c_char>(), buffer.len() as isize)
        }
    }

    /// 脚本单次 `pb.read` 的上限。
    ///
    /// 比链路层的上限大得多（脚本是"在里面"读，不用把数据搬过 socket），
    /// 但仍要有：没有上限的话一次手滑的 `pb.read(addr, 1<<40)` 会试图分配
    /// 一个天文数字的缓冲区，把目标进程的内存直接吃光。
    const MAX_SCRIPT_READ_BYTES: u64 = 4 * 1024 * 1024;

    /// `_pb_read_native` 的方法表（必须是静态：CPython 只存指针）。
    static READ_NATIVE_DEF: SyncMethodDef = SyncMethodDef(PyMethodDef {
        name: c"_pb_read_native".as_ptr(),
        method: pb_read_native,
        flags: METH_VARARGS,
        doc: c"read target memory: read(address, length) -> bytes".as_ptr(),
    });
    /// 一次脚本执行的结果。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum RunOutcome {
        /// 正常跑完。
        Completed,
        /// 超出时间预算，已被看门狗中断。
        TimedOut,
    }

    /// 一次事件分发的结果。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DispatchOutcome {
        /// 正常完成，值是被调用的订阅者数量。
        Called(u32),
        /// 超出时间预算，已被中断。
        TimedOut,
    }
    /// 一个"新引用"的 RAII 持有者：离开作用域时 `Py_DecRef`。
    ///
    /// 为什么必须有它：CPython 的引用计数一旦漏掉就变成内存泄漏，
    /// 多减一次就是目标进程里的随机崩溃。手写 `DecRef` 分散在十几个
    /// early-return 路径上几乎必然会错，所以让类型来管。
    struct Owned<'a>(&'a PyApi, *mut c_void);

    impl<'a> Owned<'a> {
        fn new(api: &'a PyApi, object: *mut c_void) -> Self {
            Self(api, object)
        }

        /// 被持有的 Python 对象指针。
        ///
        /// 用具名访问器而不是 `.1`：元组字段在调用点读起来是"某个东西的第 N 项"，
        /// 而这里要的是"那个对象"——写错过一次（把 `.0`（api 引用）当成了对象指针）。
        fn object(&self) -> *mut c_void {
            self.1
        }

        fn is_null(&self) -> bool {
            self.1.is_null()
        }
    }

    impl Drop for Owned<'_> {
        fn drop(&mut self) {
            if !self.1.is_null() {
                // SAFETY: 这个对象是 `Owned` 的唯一所有者（由 new 的调用点保证）。
                unsafe { (self.0.Py_DecRef)(self.1) };
            }
        }
    }

    /// 把 `&str` 变成 C 串（不带上下文名，调用点自己给更好的错误文本）。
    fn c_string(text: &str) -> Result<CString> {
        CString::new(text)
            .map_err(|_| Error::Script(format!("字符串含 NUL 字节，无法传给解释器：{text}")))
    }

    /// 注入 `pb` 模块的引导脚本（纯 Python，见 `PythonHost::bootstrap` 的说明）。
    ///
    /// 接口与设计文档一致：`pb.subscribe(event, handler)` / `pb.log(text)` /
    /// `pb.mark(name, address)`，事件对象用 `SimpleNamespace`（`ev.address`、
    /// `ev.args` 这种属性访问方式与文档里的示例一致）。
    const BOOTSTRAP: &str = r#"
import json as _json, sys as _sys, time as _time, types as _types

_PB_OUTBOX = []
_PB_SUBSCRIBERS = {}

def _pb_emit(kind, value, text):
    _PB_OUTBOX.append([kind, int(value), str(text)])

def _pb_log(text):
    _pb_emit(1, 0, text)

def _pb_mark(name, address=0):
    _pb_emit(2, int(address), str(name))

# 当前正在执行的脚本标记。订阅时把处理器与它绑在一起，
# 于是"换脚本/卸载"能只清掉那一个脚本留下的处理器。
_PB_TAG = ""

def _pb_set_script_tag(tag):
    global _PB_TAG
    _PB_TAG = str(tag)

def _pb_subscribe(event_name, handler=None):
    name = str(event_name)
    handlers = _PB_SUBSCRIBERS.setdefault(name, [])
    if handler is not None:
        handlers.append((_PB_TAG, handler))
    _pb_emit(3, len(handlers), name)

def _pb_drop_handlers(tag):
    """清掉某个脚本标记留下的全部处理器，返回清掉的条数。

    为什么必须有它：`pb.subscribe` 只追加。换脚本或卸载时如果不清掉旧的处理器，
    它们**仍然会被调用**——表现为"我明明改了脚本，行为却还是旧的"，
    这是最难查的一类问题（没有任何错误，只有不对的结果）。
    """
    tag = str(tag)
    removed = 0
    for name in list(_PB_SUBSCRIBERS.keys()):
        kept = [item for item in _PB_SUBSCRIBERS[name] if item[0] != tag]
        removed += len(_PB_SUBSCRIBERS[name]) - len(kept)
        _PB_SUBSCRIBERS[name] = kept
    return removed

def _pb_handler_count():
    """当前登记的处理器总数（测试与诊断用）。"""
    return sum(len(items) for items in _PB_SUBSCRIBERS.values())

_pb = _types.ModuleType("pb")
_pb.log = _pb_log
_pb.mark = _pb_mark
_pb.subscribe = _pb_subscribe
# read 由 Rust 提供（`_pb_read_native`），因为读内存必须过能力闸门——
# 闸门在 Rust 侧才够不着（脚本能改 Python 里的任何东西）。
_pb.read = _pb_read_native
_pb.__doc__ = "PinBridge 目标侧接口（由 agent 注入，不是安装到环境里的包）"
_sys.modules["pb"] = _pb

class _PBTimeout(Exception):
    """脚本超出时间预算时抛出（由宿主的看门狗线程异步注入）。

    实现说明：预算**不用 `sys.settrace`** 实现。原因有两条，都是实测得到的：

    * 超时中断过一次之后，解释器的线程跟踪状态会留下痕迹，之后再次
      `sys.settrace` 可能**收不到行事件**——看门狗于是静默失效，脚本继续死循环；
    * 关掉看门狗这一步本身也会被跟踪，于是"清理"和"看门狗生效"互相打架。

    现在由宿主起一个看门狗线程，到点用 `PyThreadState_SetAsyncExc` 把本异常
    注入目标线程。它不依赖任何解释器跟踪状态，也就没有上述两个问题。
    """

def _pb_timeout_class():
    """把超时异常类交给宿主（宿主注入时要用同一个类）。"""
    return _PBTimeout

def _pb_drain_json():
    # ensure_ascii=False：日志里的中文不该被转成 \uXXXX——
    # 这些文本是给人和 AI 读的，转义只会让它更难读。
    payload = _json.dumps(_PB_OUTBOX, ensure_ascii=False)
    del _PB_OUTBOX[:]
    return payload

def _pb_dispatch(combined_json):
    combined = _json.loads(combined_json)
    name = str(combined.get("event"))
    handlers = _PB_SUBSCRIBERS.get(name, [])
    if not handlers:
        return 0
    event = _types.SimpleNamespace(**combined.get("payload") or {})
    for item in list(handlers):
        item[1](event)
    return len(handlers)
"#;
    impl Drop for PyApi {
        fn drop(&mut self) {
            if !self.module.is_null() {
                // SAFETY: module 来自 LoadLibraryA，且只在 drop 时释放一次。
                unsafe {
                    FreeLibrary(self.module);
                }
            }
        }
    }

    fn last_error_hint() -> String {
        // 只取一个整数错误码：引入 FormatMessage 会让这里多依赖一堆东西，
        // 而这条错误信息的主要作用是**指出该装什么**，不是诊断 Windows 内部。
        format!(
            "（LoadLibraryA 失败，GetLastError 未取；请确认已安装 Python 3.x 并检查 {DLL_ENV}）"
        )
    }

    /// 按候选顺序找并加载解释器 DLL。
    fn load_python() -> Result<(PyApi, String)> {
        let mut tried: Vec<String> = Vec::new();
        if let Ok(explicit) = std::env::var(DLL_ENV) {
            if !explicit.is_empty() {
                tried.push(explicit.clone());
                match try_load(&explicit) {
                    Some(api) => return Ok((api, explicit)),
                    None => {
                        // 显式指定却加载失败：**不再回退**。回退会让"我指定的库
                        // 没生效"变成一个沉默的事实。
                        return Err(Error::TargetUnavailable(format!(
                            "按 {DLL_ENV} 指定的解释器加载失败：{explicit}{}",
                            last_error_hint()
                        )));
                    }
                }
            }
        }
        for name in CANDIDATE_DLLS {
            tried.push((*name).to_owned());
            if let Some(api) = try_load(name) {
                return Ok((api, (*name).to_owned()));
            }
        }
        Err(Error::TargetUnavailable(format!(
            "找不到可用的 CPython 解释器（试过：{}）。安装 Python 3.x，\
             或用 {DLL_ENV} 指定 python3.dll 的完整路径；\
             在此之前脚本能力不可用，其它能力不受影响。",
            tried.join(", ")
        )))
    }

    fn try_load(library: &str) -> Option<PyApi> {
        let name = CString::new(library).ok()?;
        // SAFETY: name 以 NUL 结尾且在本调用期间存活。
        let module = unsafe { LoadLibraryA(name.as_ptr()) };
        if module.is_null() {
            return None;
        }
        // SAFETY: module 来自 LoadLibraryA。每个符号都检查是否解析成功。
        unsafe {
            let mut missing: Vec<&str> = Vec::new();
            macro_rules! must {
                ($symbol:literal) => {{
                    let address = GetProcAddress(module, concat!($symbol, "\0").as_ptr().cast());
                    if address.is_null() {
                        missing.push($symbol);
                    }
                    address
                }};
            }
            let is_initialized = must!("Py_IsInitialized");
            let initialize_ex = must!("Py_InitializeEx");
            let run_simple_string = must!("PyRun_SimpleString");
            let gil_ensure = must!("PyGILState_Ensure");
            let gil_release = must!("PyGILState_Release");
            let save_thread = must!("PyEval_SaveThread");
            let import_add_module = must!("PyImport_AddModule");
            let get_attr_string = must!("PyObject_GetAttrString");
            let object_call = must!("PyObject_Call");
            let tuple_new = must!("PyTuple_New");
            let tuple_set_item = must!("PyTuple_SetItem");
            let unicode_from_string = must!("PyUnicode_FromString");
            let unicode_as_utf8 = must!("PyUnicode_AsUTF8String");
            let bytes_as_string = must!("PyBytes_AsString");
            let bytes_size = must!("PyBytes_Size");
            let dec_ref = must!("Py_DecRef");
            let object_str = must!("PyObject_Str");
            // 异常类型是**数据符号**：取到的是"指向 PyObject* 的指针"。
            let exc_runtime_error = GetProcAddress(module, c"PyExc_RuntimeError".as_ptr());
            let exc_value_error = GetProcAddress(module, c"PyExc_ValueError".as_ptr());
            if exc_runtime_error.is_null() || exc_value_error.is_null() {
                FreeLibrary(module);
                return None;
            }
            let cfunction_new = must!("PyCFunction_NewEx");
            let bytes_from_string_and_size = must!("PyBytes_FromStringAndSize");
            let tuple_get_item = must!("PyTuple_GetItem");
            let tuple_size = must!("PyTuple_Size");
            let long_as_unsigned = must!("PyLong_AsUnsignedLongLong");
            let err_set_string = must!("PyErr_SetString");
            let module_get_dict = must!("PyModule_GetDict");
            let dict_set_item_string = must!("PyDict_SetItemString");
            let thread_state_set_async_exc = must!("PyThreadState_SetAsyncExc");
            let thread_get_thread_ident = must!("PyThread_get_thread_ident");
            let err_print = must!("PyErr_Print");
            // 可选：3.13 之前没有这个符号。
            let is_finalizing =
                GetProcAddress(module, c"Py_IsFinalizing".as_ptr()).cast::<c_void>();
            if !missing.is_empty() {
                FreeLibrary(module);
                return None;
            }
            let is_finalizing: Option<unsafe extern "C" fn() -> c_int> = if is_finalizing.is_null()
            {
                None
            } else {
                Some(std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn() -> c_int,
                >(is_finalizing))
            };
            Some(PyApi {
                module,
                Py_IsInitialized: std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> c_int>(
                    is_initialized,
                ),
                Py_InitializeEx: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int)>(
                    initialize_ex,
                ),
                Py_IsFinalizing: is_finalizing,
                PyRun_SimpleString: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*const c_char) -> c_int,
                >(run_simple_string),
                PyGILState_Ensure: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn() -> c_int,
                >(gil_ensure),
                PyGILState_Release: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int)>(
                    gil_release,
                ),
                PyEval_SaveThread: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn() -> *mut c_void,
                >(save_thread),
                PyImport_AddModule: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*const c_char) -> *mut c_void,
                >(import_add_module),
                PyObject_GetAttrString: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
                >(get_attr_string),
                PyObject_Call: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void,
                >(object_call),
                PyTuple_New: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(isize) -> *mut c_void,
                >(tuple_new),
                PyTuple_SetItem: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, isize, *mut c_void) -> c_int,
                >(tuple_set_item),
                PyUnicode_FromString: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*const c_char) -> *mut c_void,
                >(unicode_from_string),
                PyUnicode_AsUTF8String: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void) -> *mut c_void,
                >(unicode_as_utf8),
                PyBytes_AsString: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void) -> *mut c_char,
                >(bytes_as_string),
                PyBytes_Size: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void) -> isize,
                >(bytes_size),
                Py_DecRef: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void)>(
                    dec_ref,
                ),
                PyErr_Print: std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(err_print),
                PyObject_Str: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void) -> *mut c_void,
                >(object_str),
                PyCFunction_NewEx: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut PyMethodDef, *mut c_void, *mut c_void) -> *mut c_void,
                >(cfunction_new),
                PyBytes_FromStringAndSize: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*const c_char, isize) -> *mut c_void,
                >(bytes_from_string_and_size),
                PyTuple_GetItem: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, isize) -> *mut c_void,
                >(tuple_get_item),
                PyTuple_Size: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void) -> isize,
                >(tuple_size),
                PyLong_AsUnsignedLongLong: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void) -> u64,
                >(long_as_unsigned),
                PyErr_SetString: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, *const c_char),
                >(err_set_string),
                // 数据符号：`GetProcAddress` 给的是"那个变量的地址"，
                // 我们要的是"变量里存的值"（指向异常类型的 `PyObject*`）。
                // 用 `cast` 而不是 `transmute`：这是一次指针类型转换，
                // 不是跨类型重解释。
                PyExc_RuntimeError: exc_runtime_error.cast::<*mut c_void>(),
                PyExc_ValueError: exc_value_error.cast::<*mut c_void>(),
                PyModule_GetDict: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void) -> *mut c_void,
                >(module_get_dict),
                PyThreadState_SetAsyncExc: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(std::os::raw::c_ulong, *mut c_void) -> c_int,
                >(thread_state_set_async_exc),
                PyThread_get_thread_ident: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn() -> std::os::raw::c_ulong,
                >(thread_get_thread_ident),
                PyDict_SetItemString: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> c_int,
                >(dict_set_item_string),
            })
        }
    }

    /// 已初始化的解释器句柄。
    ///
    /// 单例：CPython 在一个进程里只能初始化一次，而 agent 的导出面是全局的。
    static HOST: std::sync::OnceLock<std::sync::Mutex<PythonHost>> = std::sync::OnceLock::new();

    /// 解释器宿主。
    ///
    /// 只在模块内部（以及单测）使用：对外暴露的是 [`library_description`]
    /// 与 [`probe`]，它们把平台差异与锁的细节挡在里面。
    pub(crate) struct PythonHost {
        /// `None` 表示启动失败（原因在 `failure`）。
        ///
        /// 用 `Option` 而不是"一份全是空壳函数指针的 `PyApi`"：后者要在每新增
        /// 一个符号时补一个假实现，而且"调用到空壳"这件事只能靠约定避免。
        /// `Option` 让"没启动成功"在类型上就不可调用。
        api: Option<PyApi>,
        library: String,
        /// 我们是否调用了 `Py_InitializeEx`（决定收尾时要不要 `Py_FinalizeEx`）。
        ///
        /// 如果进程里已经有别的代码初始化过解释器（例如目标自己嵌了 Python），
        /// 我们**不是**它的主人，就绝不能去 finalize 它——那会把目标的解释器
        /// 拆掉，表现为"目标在某个时刻莫名崩了"。
        we_initialized: bool,
        /// 启动失败的原因（成功后为 `None`）。
        failure: Option<String>,
        /// `PyEval_SaveThread` 返回的线程状态。
        ///
        /// 保存它是因为**这是把 GIL 交出去的那一步的凭据**。我们永远不 finalize
        /// （见 Drop），所以不需要用它恢复；但把它留在结构体里，
        /// 「GIL 已经被交出去了」这件事就有了一个可读的载体。
        /// 读取它的地方是 [`Self::gil_was_released`]。
        saved_thread: *mut c_void,
    }

    // SAFETY: `saved_thread` 只是 `PyEval_SaveThread` 返回值的凭据，
    // 不会被跨线程解引用；解释器的线程安全由 **GIL** 保证——每次调用
    // 都成对 `PyGILState_Ensure` / `PyGILState_Release`。
    // `PyApi` 里的函数指针本身也不访问线程局部状态。
    unsafe impl Send for PythonHost {}
    unsafe impl Sync for PythonHost {}
    impl std::fmt::Debug for PythonHost {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PythonHost")
                .field("library", &self.library)
                .field("we_initialized", &self.we_initialized)
                .field("failure", &self.failure)
                // `finish_non_exhaustive`：`api` 是函数指针表，打出来只是一串地址，
                // 对读日志的人没有价值。显式标注"还有字段没打"，而不是让 clippy
                // 或者后来的人以为这个结构体只有三个字段。
                .finish_non_exhaustive()
        }
    }

    impl PythonHost {
        /// 加载并（必要时）初始化解释器。
        ///
        /// # Errors
        ///
        /// 找不到解释器、或初始化失败时返回可行动的错误。
        pub(crate) fn start() -> Result<Self> {
            let (api, library) = load_python()?;
            // SAFETY: 只查询状态。
            let already = unsafe { (api.Py_IsInitialized)() } != 0;
            let mut saved_thread = std::ptr::null_mut();
            if !already {
                // 参数 0：**不接管信号处理**。目标进程的信号/异常处理是它的，
                // 我们替它装一套会让目标行为发生它自己都不知道的改变。
                // SAFETY: 调用方保证这是进程内第一次初始化解释器（上面刚查过）。
                unsafe { (api.Py_InitializeEx)(0) };
                // SAFETY: 同上。
                let now = unsafe { (api.Py_IsInitialized)() } != 0;
                if !now {
                    return Err(Error::TargetUnavailable(format!(
                        "加载到 {library} 但 Py_InitializeEx 之后解释器仍未就绪"
                    )));
                }
                // **初始化线程必须把 GIL 交出来**。
                //
                // 否则它一直持有 GIL，而任何别的线程一调 `PyGILState_Ensure`
                // 就会永远等下去。这不是理论问题：实测过一次完整的死锁——
                // cargo 的主线程初始化了解释器（于是持有 GIL），然后在等测试线程，
                // 而测试线程正在 `PyGILState_Ensure` 上等主线程，测试进程挂住 10 分钟。
                //
                // 生产路径同理：探针回调跑在**目标线程**上，初始化线程绝不能霸着 GIL。
                //
                // SAFETY: 解释器刚初始化完，当前线程持有 GIL；SaveThread 之后
                // 由 PyGILState_Ensure/Release 成对管理。返回的线程状态我们保存着——
                // 因为**我们永远不 finalize**（见 Drop 的说明），它不需要被恢复使用。
                saved_thread = unsafe { (api.PyEval_SaveThread)() };
            }
            // 回调路径要用的第二份函数表（见 `CALLBACK_API` 的说明）。
            //
            // 这里再 `LoadLibraryA` 一次同一份 DLL：拿到的是一个**独立的引用**，
            // 也就避免了"回调必须去抢宿主互斥锁"这条死锁路径。
            // 设置失败（例如另一条线程先设上了）就把这份丢掉——DLL 的引用计数
            // 会退回，不影响任何东西。
            if CALLBACK_API.get().is_none() {
                if let Some(extra) = try_load(&library) {
                    let _ = CALLBACK_API.set(extra);
                }
            }
            Ok(Self {
                api: Some(api),
                library,
                we_initialized: !already,
                failure: None,
                saved_thread,
            })
        }

        /// 实际加载的解释器库。
        ///
        /// 这一层能力分两步落地：**解释器宿主**（已可用）与
        /// `pb` 模块/回调分发（下一步）。下面几个方法已经被单测覆盖，
        /// 但还没有生产调用方——显式标注而不是删掉再由下一步重写一遍。
        #[must_use]
        #[allow(dead_code)]
        pub(crate) fn library(&self) -> &str {
            &self.library
        }

        /// GIL 是否已经在初始化后被交出去。
        ///
        /// 生产代码不需要它，但它是**可验证的性质**：一个"没交出去"的宿主意味着
        /// 任何别的线程一调解释器就会死锁，而这件事在默认的测试输出里是看不出来的。
        /// 单测直接断言它，比写一段注释解释"我们记得交出去"更可靠。
        #[must_use]
        #[allow(dead_code)]
        pub(crate) fn gil_was_released(&self) -> bool {
            !self.saved_thread.is_null()
        }

        /// 我们是否是解释器的主人。
        #[must_use]
        #[allow(dead_code)]
        pub(crate) const fn owns_interpreter(&self) -> bool {
            self.we_initialized
        }

        /// 跑一段脚本，返回解释器的状态码（0 = 成功）。
        ///
        /// # Errors
        ///
        /// 脚本里有语法错误或未捕获异常时，CPython 打印到 stderr 并返回非 0；
        /// 这里把它翻译成一个**带原文**的错误，而不是让调用方去猜。
        /// 注入 `pb` 模块并装好分发入口。
        ///
        /// 用**纯 Python** 实现 `pb`，而不是写一个 C 扩展模块：C 扩展要自己摆
        /// `PyModuleDef`/`PyMethodDef` 的结构体布局，写错就是目标进程里的随机崩溃；
        /// 而这一层需要的东西（收集输出、按事件名分发、把 payload 变成对象）
        /// 用 Python 表达更短也更清楚。Rust 只保留两个窄入口：
        /// `_pb_drain_json()` 与 `_pb_dispatch(name, payload_json)`。
        ///
        /// 幂等：重复调用只会覆盖同名入口。
        pub(crate) fn bootstrap(&self) -> Result<()> {
            self.install_host_callables()?;
            self.run(BOOTSTRAP)?;
            self.remember_timeout_class()
        }

        /// 把 `_PBTimeout` 类的引用记下来（看门狗注入时要用同一个类）。
        ///
        /// 只取一次：类由 `__main__` 持有、随进程结束消失，所以我们**不**减引用
        /// （减了会让一个还活着的对象被回收）。
        fn remember_timeout_class(&self) -> Result<()> {
            if TIMEOUT_CLASS.get().is_some() {
                return Ok(());
            }
            let api = self.api()?;
            // SAFETY: 全程持有 GIL。
            let gil = unsafe { (api.PyGILState_Ensure)() };
            let fetched = (|| -> Result<usize> {
                // SAFETY: main 返回借用引用。
                let main = unsafe { (api.PyImport_AddModule)(c"__main__".as_ptr()) };
                if main.is_null() {
                    return Err(Error::Script("取不到 __main__".into()));
                }
                // SAFETY: 返回新引用，交给 `Owned`。
                let getter = Owned::new(api, unsafe {
                    (api.PyObject_GetAttrString)(main, c"_pb_timeout_class".as_ptr())
                });
                if getter.is_null() {
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script("引导脚本没有提供 _pb_timeout_class".into()));
                }
                // **零参数也必须给一个空元组**：`PyObject_Call` 的 args 不允许
                // 是 NULL（传 NULL 会把解释器打崩——实测过一次）。kwargs 才可以是 NULL。
                // SAFETY: 空的参数元组；返回值是新引用（就是那个类对象，我们留着）。
                let empty = Owned::new(api, unsafe { (api.PyTuple_New)(0) });
                if empty.is_null() {
                    return Err(Error::Script("构造空参数元组失败".into()));
                }
                // SAFETY: getter 可调用；empty 是合法元组；kwargs 传 NULL 表示没有关键字参数。
                let class = unsafe {
                    (api.PyObject_Call)(getter.object(), empty.object(), std::ptr::null_mut())
                };
                if class.is_null() {
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script("取超时异常类失败".into()));
                }
                Ok(class as usize)
            })();
            // SAFETY: gil 来自上面的 Ensure。
            unsafe { (api.PyGILState_Release)(gil) };
            let address = fetched?;
            let _ = TIMEOUT_CLASS.set(address);
            Ok(())
        }

        /// 把 Rust 支撑的可调用对象放进 `__main__`，再让引导脚本把它接到
        /// `pb` 模块上。
        ///
        /// 分工是刻意的：**对外长什么样由 Python 决定**（名字、docstring、
        /// 包装逻辑都可以在 Python 里改），Rust 只提供最小的原生函数。
        fn install_host_callables(&self) -> Result<()> {
            let api = self.api()?;
            // SAFETY: 全程持有 GIL；返回值全部交给 `Owned`。
            let gil = unsafe { (api.PyGILState_Ensure)() };
            let result = (|| -> Result<()> {
                // 静态方法表只用作参数：`PyCFunction_NewEx` 只存指针，不拷贝，
                // 所以它必须活到进程结束（见 `READ_NATIVE_DEF` 的说明）。
                let callable = Owned::new(
                    api,
                    // SAFETY: 方法表是静态的，会活到进程结束；self/module 传空。
                    unsafe {
                        (api.PyCFunction_NewEx)(
                            std::ptr::addr_of!(READ_NATIVE_DEF.0).cast_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        )
                    },
                );
                if callable.is_null() {
                    // SAFETY: 打印并清掉 CPython 侧的错误信息。
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script("构造原生可调用对象失败".into()));
                }
                // SAFETY: `PyImport_AddModule` 返回借用引用。
                let main = unsafe { (api.PyImport_AddModule)(c"__main__".as_ptr()) };
                if main.is_null() {
                    return Err(Error::Script("取不到 __main__ 模块".into()));
                }
                // SAFETY: main 有效；返回借用引用。
                let dict = unsafe { (api.PyModule_GetDict)(main) };
                if dict.is_null() {
                    return Err(Error::Script("取不到 __main__ 的字典".into()));
                }
                // `PyDict_SetItemString` 会自己 incref（不偷引用），
                // 所以 `callable` 仍然由 `Owned` 负责释放。
                // SAFETY: dict/callable 有效，键是 NUL 结尾的字面量。
                let code = unsafe {
                    (api.PyDict_SetItemString)(dict, c"_pb_read_native".as_ptr(), callable.object())
                };
                if code != 0 {
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script("把原生可调用对象放进 __main__ 失败".into()));
                }
                Ok(())
            })();
            // SAFETY: gil 来自上面的 Ensure。
            unsafe { (api.PyGILState_Release)(gil) };
            result
        }

        /// 调用 `__main__` 里的一个函数，可选传一个字符串参数，返回其字符串结果。
        ///
        /// 只支持"零参或一个字符串参数、返回字符串"——这是本层与脚本之间
        /// **唯一的约定**，刻意保持窄：任何更复杂的数据交换都应该走 JSON。
        ///
        /// # Errors
        ///
        /// 函数不存在、调用抛异常、或结果不是可转成 UTF-8 的字符串时出错。
        pub(crate) fn call_json(&self, function: &str, argument: Option<&str>) -> Result<String> {
            let api = self.api()?;
            let function_c = c_string(function)?;
            // SAFETY: 全程持有 GIL；每一个"新引用"都由 `Owned` 负责释放。
            let gil = unsafe { (api.PyGILState_Ensure)() };
            let result = (|| -> Result<String> {
                // SAFETY: `PyImport_AddModule` 返回**借用**引用（不要 DecRef）。
                let main = unsafe { (api.PyImport_AddModule)(c"__main__".as_ptr()) };
                if main.is_null() {
                    return Err(Error::Script("取不到 __main__ 模块".into()));
                }
                // SAFETY: main 有效；返回新引用，交给 Owned。
                let callable = Owned::new(api, unsafe {
                    (api.PyObject_GetAttrString)(main, function_c.as_ptr())
                });
                if callable.is_null() {
                    // 属性不存在：把 Python 的异常信息打出来，再报一个清楚的错。
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script(format!(
                        "脚本没有定义 {function}（本层需要它作为入口）"
                    )));
                }
                // 构造参数元组。`PyTuple_SetItem` **偷引用**，所以字符串对象不再由我们释放。
                // SAFETY: 见上。
                let mut tuple_size: isize = 0;
                if argument.is_some() {
                    tuple_size = 1;
                }
                let args = Owned::new(api, unsafe { (api.PyTuple_New)(tuple_size) });
                if args.is_null() {
                    return Err(Error::Script("构造参数元组失败".into()));
                }
                if let Some(text) = argument {
                    let text_c = c_string(text)?;
                    // SAFETY: text_c 在本调用期间存活；返回新引用。
                    let value = unsafe { (api.PyUnicode_FromString)(text_c.as_ptr()) };
                    if value.is_null() {
                        return Err(Error::Script("构造字符串参数失败".into()));
                    }
                    // SAFETY: 元组长度是 1，索引 0 合法；SetItem 取得所有权。
                    if unsafe { (api.PyTuple_SetItem)(args.object(), 0, value) } != 0 {
                        return Err(Error::Script("写入参数元组失败".into()));
                    }
                }
                // SAFETY: callable 与 args 都有效；kwargs 传空指针表示"没有关键字参数"。
                let returned = Owned::new(api, unsafe {
                    (api.PyObject_Call)(callable.object(), args.object(), std::ptr::null_mut())
                });
                if returned.is_null() {
                    // 脚本里抛了异常：让 CPython 打印堆栈（带行号），再返回错误。
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script(format!("调用 {function} 时脚本抛出了异常")));
                }
                // 结果转文本分两步：任意对象 → `str`（`PyObject_Str`）→ UTF-8 bytes。
                //
                // 中间这一步是必需的：脚本返回 `int` 这类**非字符串**结果是正常用法
                // （例如 `_pb_dispatch` 返回被调用的订阅者数量），
                // 直接对 int 调 `PyUnicode_AsUTF8String` 会抛 TypeError，
                // 而错误文本会变成一句与真实原因无关的"返回值不是字符串"。
                //
                // SAFETY: returned 是有效对象；下面每一步都检查了空。
                let as_text = Owned::new(api, unsafe { (api.PyObject_Str)(returned.object()) });
                if as_text.is_null() {
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script(format!("{function} 的返回值无法转成文本")));
                }
                // SAFETY: as_text 是 str 对象。
                let as_bytes = Owned::new(api, unsafe {
                    (api.PyUnicode_AsUTF8String)(as_text.object())
                });
                if as_bytes.is_null() {
                    unsafe { (api.PyErr_Print)() };
                    return Err(Error::Script(format!("{function} 的返回值不是字符串")));
                }
                // SAFETY: as_bytes 是 bytes 对象。
                let pointer = unsafe { (api.PyBytes_AsString)(as_bytes.object()) };
                let length = unsafe { (api.PyBytes_Size)(as_bytes.object()) };
                if pointer.is_null() || length < 0 {
                    return Err(Error::Script(format!("读取 {function} 的返回值失败")));
                }
                // SAFETY: pointer/length 来自同一个 bytes 对象，且在本作用域内有效。
                let slice =
                    unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length as usize) };
                Ok(String::from_utf8_lossy(slice).into_owned())
            })();
            // SAFETY: gil 来自上面的 Ensure。
            unsafe { (api.PyGILState_Release)(gil) };
            result
        }

        /// 带时间预算地执行一段脚本。
        ///
        /// 返回 `TimedOut` 而不是普通错误：调用方要能区分"脚本写错了"与
        /// "脚本太慢/死循环了"——两者的处置完全不同（改脚本 vs 加预算/换算法）。
        ///
        /// 预算用**行级跟踪**（`sys.settrace`）实现：Python 里死循环不会自己
        /// 让出控制权，而跟踪函数每个行事件都会被调用一次，于是"只有一行的死循环"
        /// 也一定能被检查到。代价是脚本执行期间的额外开销——所以只在脚本运行时
        /// 打开，运行结束（含出错）立刻关掉。
        pub(crate) fn run_with_budget(&self, source: &str, budget_ms: u64) -> Result<RunOutcome> {
            let generation = self.arm_budget(budget_ms)?;
            let ran = self.run(source);
            // **无论成功失败都要撤掉预算**：留着会让看门狗把一次已经结束的
            // 调用的超时算到后面某次调用头上。
            let fired = self.disarm_budget(generation)?;
            match ran {
                Ok(()) => Ok(if fired {
                    RunOutcome::TimedOut
                } else {
                    RunOutcome::Completed
                }),
                // 超时是**以异常的形式**从解释器里冒出来的（看门狗抛 `_PBTimeout`），
                // 所以这里必须问一次"是不是超时"再决定怎么报：
                // 直接把它当普通脚本错误返回，AI 会把"死循环被中断"误读成"脚本有 bug"，
                // 然后去改一段本来没错的代码。
                // 超时是**以异常的形式**从解释器里冒出来的（看门狗注入的
                // `_PBTimeout`），所以这里必须看一次"看门狗有没有开过枪"再决定
                // 怎么报：直接当普通脚本错误返回，AI 会把"死循环被中断"
                // 误读成"脚本有 bug"，然后去改一段本来没错的代码。
                Err(error) => {
                    if fired {
                        Ok(RunOutcome::TimedOut)
                    } else {
                        Err(error)
                    }
                }
            }
        }

        /// 带时间预算地把事件分发给脚本。
        pub(crate) fn dispatch_with_budget(
            &self,
            event_name: &str,
            payload_json: &str,
            budget_ms: u64,
        ) -> Result<DispatchOutcome> {
            let generation = self.arm_budget(budget_ms)?;
            let dispatched = self.dispatch(event_name, payload_json);
            let fired = self.disarm_budget(generation)?;
            // 超时：脚本已经被中断，"被调用过几个订阅者"这件事不再重要，
            // 重要的是如实报告它超时了。
            if fired {
                return Ok(DispatchOutcome::TimedOut);
            }
            dispatched.map(DispatchOutcome::Called)
        }

        /// 给接下来执行的脚本打标记，并清掉**同一个标记**留下的旧处理器。
        ///
        /// 顺序是刻意的：先清旧的、再执行新的，于是"同名脚本换版本"的效果是
        /// 旧行为完全消失、新行为生效——而不是两套处理器同时活着。
        pub(crate) fn set_script_tag(&self, tag: &str) -> Result<()> {
            self.call_json("_pb_set_script_tag", Some(tag))
                .map(|_| ())?;
            // 清掉这个标记的旧处理器（首次加载时是空操作）。
            self.call_json("_pb_drop_handlers", Some(tag)).map(|_| ())?;
            Ok(())
        }

        /// 清掉某个标记留下的处理器（卸载脚本时用）。
        pub(crate) fn drop_handlers(&self, tag: &str) -> Result<u32> {
            let counted = self.call_json("_pb_drop_handlers", Some(tag))?;
            Ok(counted.trim().parse().unwrap_or(0))
        }

        /// 当前登记的处理器总数。
        pub(crate) fn handler_count(&self) -> Result<u32> {
            let counted = self.call_json("_pb_handler_count", None)?;
            Ok(counted.trim().parse().unwrap_or(0))
        }
        /// 装预算并清掉上一次的超时计数。
        /// 装预算，返回本次调用的"代号"（disarm 时用它对账）。
        fn arm_budget(&self, budget_ms: u64) -> Result<u64> {
            if budget_ms == 0 {
                return Ok(0); // 0 = 不限制
            }
            let api = self.api()?;
            // SAFETY: 取当前线程的解释器标识，只在同进程内使用。
            let ident = unsafe { (api.PyThread_get_thread_ident)() };
            let dog = watchdog();
            let mut guard = dog
                .inner
                .lock()
                .map_err(|_| Error::Script("看门狗锁中毒".into()))?;
            guard.generation = guard.generation.wrapping_add(1);
            guard.fired = false;
            guard.target = Some((
                ident,
                std::time::Instant::now() + std::time::Duration::from_millis(budget_ms),
            ));
            let generation = guard.generation;
            drop(guard);
            dog.signal.notify_all();
            Ok(generation)
        }

        /// 撤掉预算；返回**这次调用是否被看门狗注入过超时**。
        ///
        /// 超时与否由看门狗自己的状态回答，不再问解释器：这样"超时判定"
        /// 不依赖任何 Python 侧状态，也就不会出现"清理与判定互相打架"。
        fn disarm_budget(&self, generation: u64) -> Result<bool> {
            if generation == 0 {
                return Ok(false);
            }
            let dog = watchdog();
            let mut guard = dog
                .inner
                .lock()
                .map_err(|_| Error::Script("看门狗锁中毒".into()))?;
            let fired = guard.generation == generation && guard.fired;
            if guard.generation == generation {
                guard.target = None;
            }
            Ok(fired)
        }

        /// 取走脚本产生的输出（JSON 数组），并清空脚本侧队列。
        pub(crate) fn drain_outputs(&self) -> Result<String> {
            self.call_json("_pb_drain_json", None)
        }

        /// 把一个事件交给脚本的订阅者，返回被调用的订阅者数量。
        ///
        /// 两个字符串打包成一个 JSON 再传：`call_json` 的约定是"零参或一个字符串
        /// 参数"，把它的约定撑宽到多参数会让这一层多出一堆位置错误的机会。
        pub(crate) fn dispatch(&self, event_name: &str, payload_json: &str) -> Result<u32> {
            let combined = serde_json::json!({
                "event": event_name,
                "payload": serde_json::from_str::<serde_json::Value>(payload_json)
                    .unwrap_or(serde_json::Value::Null),
            })
            .to_string();
            let counted = self.call_json("_pb_dispatch", Some(&combined))?;
            counted
                .trim()
                .parse::<u32>()
                .map_err(|_| Error::Script(format!("_pb_dispatch 返回了非数字：{counted}")))
        }
        /// 取函数指针表；启动失败时给出原因。
        fn api(&self) -> Result<&PyApi> {
            self.api.as_ref().ok_or_else(|| {
                Error::TargetUnavailable(
                    self.failure
                        .clone()
                        .unwrap_or_else(|| "解释器未启动".to_owned()),
                )
            })
        }
        pub(crate) fn run(&self, source: &str) -> Result<()> {
            let code = CString::new(source).map_err(|_| {
                Error::Protocol("脚本里含 NUL 字节，无法作为 C 串传给解释器".into())
            })?;
            // **必须持有 GIL**：解释器初始化之后只有初始化它的那个线程持有 GIL，
            // 而调用我们的是**任意线程**（agent 的探针回调在目标线程上跑，
            // 测试里 cargo 每条用例也各在自己的线程上）。
            // 不在 GIL 保护下执行解释器代码 = 内存损坏，表现为随机访问违例。
            //
            // SAFETY: `PyGILState_Ensure` 返回的状态必须原样交给 Release；
            // 两者成对出现在同一作用域内，中途不返回错误（下面拿到状态码之后再释放）。
            let api = self.api()?;
            let gil = unsafe { (api.PyGILState_Ensure)() };
            // SAFETY: code 以 NUL 结尾且在本调用期间存活；解释器已初始化；
            // 当前线程已持有 GIL。
            let status = unsafe { (api.PyRun_SimpleString)(code.as_ptr()) };
            if status != 0 {
                // **打印并清掉**待处理异常。`PyErr_Print` 两件事都做：
                // 把 CPython 的堆栈（带行号）打到 stderr，并清空当前异常。
                //
                // 清空这一步是必需的、不是可选的：异常留着的话，调用方**随后**
                // 任何一次回到解释器的调用（例如"把看门狗关掉"）都会失败。
                // 实测过一次：`sys.settrace` 的看门狗因此留在了那条线程上，
                // 之后在这条线程上跑的每个脚本每一行都抛超时，整个测试套件挂死。
                // SAFETY: 只读取并清空当前线程的异常状态。
                unsafe { (api.PyErr_Print)() };
            }
            // SAFETY: gil 来自上面的 Ensure。
            unsafe { (api.PyGILState_Release)(gil) };
            if status != 0 {
                return Err(Error::Script(format!(
                    "脚本执行失败（解释器返回 {status}；具体错误已由 CPython 打印到 stderr）"
                )));
            }
            Ok(())
        }

        /// 解释器是否正在收尾。
        #[must_use]
        #[allow(dead_code)]
        pub(crate) fn is_finalizing(&self) -> bool {
            let Some(api) = self.api.as_ref() else {
                // 启动失败的宿主谈不上"正在收尾"。
                return false;
            };
            match api.Py_IsFinalizing {
                // SAFETY: 只查询状态。
                Some(function) => (unsafe { function() }) != 0,
                None => false,
            }
        }
    }

    impl PythonHost {
        /// 构造一个"启动失败"的宿主。
        ///
        /// 为什么要留一个失败的宿主而不是直接返回错误：`shared()` 用
        /// `OnceLock` 缓存它，这样缺 Python 的机器上**每次调用不会重复
        /// 尝试加载 DLL**（那是一串注定失败的系统调用）。
        fn failed(cause: &pinbridge_core::error::Error) -> Self {
            // 用一个空壳 PyApi：module 为空指针，Drop 时不会 FreeLibrary。
            Self {
                api: None,
                library: String::new(),
                we_initialized: false,
                failure: Some(cause.to_string()),
                saved_thread: std::ptr::null_mut(),
            }
        }

        /// 这个宿主是否是失败的（附原因）。
        #[must_use]
        pub(crate) fn failure(&self) -> Option<&str> {
            self.failure.as_deref()
        }
    }

    impl Drop for PythonHost {
        /// **解释器不在这里收尾。**
        ///
        /// 这是刻意的，而且是一条重要的正确性决定：
        ///
        /// * CPython 的解释器是**进程级**的。`Py_FinalizeEx` 会把整个进程的解释器
        ///   拆掉——包括**目标自己嵌的那一个**（被分析的程序完全可能自带 Python）。
        /// * 拆解释器还会销毁 GIL 与所有解释器状态，而其它线程/回调可能此刻仍持有
        ///   对它的引用。表现为"目标在某个时刻莫名崩了"，且现场离原因很远。
        ///
        /// 实测依据：加上 `Py_FinalizeEx` 之后，同一进程里的多条用例会互相把解释器
        /// 拆掉，整个测试进程直接 `STATUS_ACCESS_VIOLATION`。
        ///
        /// 代价是解释器的内存与状态保留到进程结束——这正是我们想要的：
        /// agent 的生命周期就等于目标的剩余生命周期。**Drop 只负责释放本结构体
        /// 自己那次 `LoadLibraryA` 的引用计数，不碰解释器。**
        fn drop(&mut self) {
            // `api` 的 Drop 会 FreeLibrary；静态持有的那份引用保证 DLL 仍然驻留。
        }
    }

    /// 探测可用性（不初始化解释器）。
    ///
    /// 两条刻意的行为：
    ///
    /// 1. **结果缓存**：探测就是一次 `LoadLibraryA` + 一串 `GetProcAddress`，
    ///    没有理由每次调用都做一遍；
    /// 2. **不卸载**：加载成功之后不再 `FreeLibrary`。卸载一个可能正被使用的
    ///    解释器是崩溃来源——实测过一次：探测把 `python314.dll` 卸掉的同时
    ///    另一个用例的解释器正活着，整个测试进程直接异常退出。
    ///
    /// "不卸载"是用**静态持有**表达的：静态的生命周期就是进程的生命周期，
    /// 于是这件事被写成了类型，而不是靠一句 `mem::forget` 去绕（仓库的 lint 表
    /// 禁止后者，理由是句柄泄漏会表现为 Pin 侧无法 re-attach）。
    #[must_use]
    pub fn probe() -> Availability {
        match loaded() {
            Ok((_, library)) => Availability::Ready {
                library: library.clone(),
            },
            Err(reason) => Availability::Unavailable {
                reason: reason.clone(),
            },
        }
    }

    /// 进程级持有：加载成功后的解释器句柄留在这里，直到进程结束。
    static LOADED: std::sync::OnceLock<std::result::Result<(PyApi, String), String>> =
        std::sync::OnceLock::new();

    fn loaded() -> &'static std::result::Result<(PyApi, String), String> {
        LOADED.get_or_init(|| match load_python() {
            Ok((api, library)) => Ok((api, library)),
            Err(error) => Err(error.to_string()),
        })
    }

    /// 注入 `pb` 模块并装好分发入口（平台统一入口）。
    ///
    /// # Errors
    ///
    /// 解释器不可用时返回带原因的错误。
    pub fn bootstrap() -> Result<()> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.bootstrap()
    }

    /// 执行一段脚本源码（平台统一入口）。
    ///
    /// # Errors
    ///
    /// 解释器不可用、或脚本抛异常时出错。
    pub fn run_source(source: &str) -> Result<()> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.run(source)
    }

    /// 带时间预算地执行一段脚本（平台统一入口）。
    ///
    /// # Errors
    ///
    /// 解释器不可用或脚本自身出错时出错；超时通过返回值表达
    /// （`TimedOut`），不走错误通道——调用方需要能区分这两件事。
    pub fn run_source_with_budget(source: &str, budget_ms: u64) -> Result<super::HostRunOutcome> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.run_with_budget(source, budget_ms).map(Into::into)
    }

    /// 带时间预算地把事件分发给脚本（平台统一入口）。
    ///
    /// # Errors
    ///
    /// 解释器不可用或脚本抛异常时出错。
    pub fn dispatch_with_budget(
        event_name: &str,
        payload_json: &str,
        budget_ms: u64,
    ) -> Result<super::HostDispatchOutcome> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard
            .dispatch_with_budget(event_name, payload_json, budget_ms)
            .map(Into::into)
    }

    /// 给脚本打标记并清掉同名脚本留下的旧处理器（平台统一入口）。
    ///
    /// # Errors
    ///
    /// 解释器不可用时出错。
    pub fn set_script_tag(tag: &str) -> Result<()> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.set_script_tag(tag)
    }

    /// 清掉某个脚本留下的处理器，返回清掉的条数（平台统一入口）。
    ///
    /// # Errors
    ///
    /// 解释器不可用时出错。
    pub fn drop_handlers(tag: &str) -> Result<u32> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.drop_handlers(tag)
    }

    /// 当前登记的处理器总数（平台统一入口）。
    ///
    /// # Errors
    ///
    /// 解释器不可用时出错。
    pub fn handler_count() -> Result<u32> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.handler_count()
    }

    /// 取走脚本产生的输出（JSON 文本）。
    ///
    /// # Errors
    ///
    /// 解释器不可用或调用失败时出错。
    pub fn drain_outputs() -> Result<String> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.drain_outputs()
    }

    /// 把一个事件交给脚本的订阅者。
    ///
    /// # Errors
    ///
    /// 解释器不可用或脚本抛异常时出错。
    pub fn dispatch(event_name: &str, payload_json: &str) -> Result<u32> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒".into()))?;
        guard.dispatch(event_name, payload_json)
    }
    /// 解释器是否可用；可用时返回它的描述（库名）。
    ///
    /// 这是跨平台统一的入口：`python_host` 内部在 Windows 与其它平台上的实现
    /// 完全不同，而调用方（导出面）不该为此写 `cfg` 分支——那种分支会长出
    /// "某个平台上少检查一次"的漏洞。
    ///
    /// # Errors
    ///
    /// 解释器不可用时返回带原因的错误。
    pub fn library_description() -> Result<String> {
        let host = shared()?;
        let guard = host
            .lock()
            .map_err(|_| Error::Script("解释器宿主锁中毒（有线程在持锁时 panic）".into()))?;
        Ok(guard.library().to_owned())
    }

    /// 进程级单例：重复调用返回同一个宿主。
    ///
    /// # Errors
    ///
    /// 解释器不可用时返回可行动的错误。
    pub(crate) fn shared() -> Result<&'static std::sync::Mutex<PythonHost>> {
        let cell = HOST.get_or_init(|| {
            // 初始化失败也要留下一个"失败的宿主"，否则每次调用都会重试加载 DLL，
            // 而缺 Python 的机器上那是纯粹的重复开销。
            let host = PythonHost::start().unwrap_or_else(|error| PythonHost::failed(&error));
            std::sync::Mutex::new(host)
        });
        if let Some(reason) = cell
            .lock()
            .map_or(None, |host| host.failure().map(str::to_owned))
        {
            return Err(Error::TargetUnavailable(reason));
        }
        Ok(cell)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Availability;
    use pinbridge_core::error::{Error, Result};

    /// 非 Windows 上没有实现动态加载（M1 范围）。
    #[must_use]
    pub fn probe() -> Availability {
        Availability::Unavailable {
            reason: "当前平台未实现 CPython 动态加载（M1 只支持 Windows）".into(),
        }
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn library_description() -> Result<String> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn bootstrap() -> Result<()> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn run_source(_source: &str) -> Result<()> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn drain_outputs() -> Result<String> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn dispatch(_event_name: &str, _payload_json: &str) -> Result<u32> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn set_script_tag(_tag: &str) -> Result<()> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn drop_handlers(_tag: &str) -> Result<u32> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn handler_count() -> Result<u32> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn run_source_with_budget(_source: &str, _budget_ms: u64) -> Result<super::HostRunOutcome> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }

    /// # Errors
    ///
    /// 总是返回"不支持"。
    pub fn dispatch_with_budget(
        _event_name: &str,
        _payload_json: &str,
        _budget_ms: u64,
    ) -> Result<super::HostDispatchOutcome> {
        Err(Error::TargetUnavailable(NOT_SUPPORTED.into()))
    }
    /// 非 Windows 平台的统一原因文本。
    const NOT_SUPPORTED: &str = "当前平台未实现 CPython 动态加载（M1 只支持 Windows）";
}

/// 脚本执行结果（跨平台统一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRunOutcome {
    /// 正常跑完。
    Completed,
    /// 超出时间预算，已被中断。
    TimedOut,
}

/// 事件分发结果（跨平台统一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDispatchOutcome {
    /// 正常完成，值是被调用的订阅者数量。
    Called(u32),
    /// 超出时间预算，已被中断。
    TimedOut,
}

// 只有 Windows 的实现才有内部结果类型，转换也只在那个平台上存在。
// （非 Windows 侧的统一入口直接返回"不支持"，压根不会走到转换。）
#[cfg(windows)]
mod outcome_bridge {
    use super::{imp, HostDispatchOutcome, HostRunOutcome};

    impl From<imp::RunOutcome> for HostRunOutcome {
        fn from(value: imp::RunOutcome) -> Self {
            match value {
                imp::RunOutcome::Completed => Self::Completed,
                imp::RunOutcome::TimedOut => Self::TimedOut,
            }
        }
    }

    impl From<imp::DispatchOutcome> for HostDispatchOutcome {
        fn from(value: imp::DispatchOutcome) -> Self {
            match value {
                imp::DispatchOutcome::Called(count) => Self::Called(count),
                imp::DispatchOutcome::TimedOut => Self::TimedOut,
            }
        }
    }
}

pub use imp::{
    bootstrap, dispatch, dispatch_with_budget, drain_outputs, drop_handlers, handler_count,
    library_description, probe, run_source, run_source_with_budget, set_script_tag,
};

/// 当前平台是否实现了 CPython 宿主。
#[must_use]
pub const fn supported() -> bool {
    cfg!(windows)
}

#[cfg(all(test, windows))]
mod tests {
    use super::imp::{DispatchOutcome, RunOutcome};
    use super::*;

    /// 串行化：CPython **不能**在同一进程里并发初始化/收尾，
    /// 而下面几条用例都会碰它（还会改环境变量）。这个锁是仓库里既有的约定。
    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        crate::host::GLOBAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn probe_reports_ready_or_a_reason_never_a_panic() {
        let _guard = serialize();
        let availability = probe();
        match availability {
            Availability::Ready { library } => {
                println!("✓ 找到解释器：{library}");
            }
            Availability::Unavailable { reason } => {
                // 这条分支在没装 Python 的机器上会走到，它**不是失败**：
                // 要求是"给出可行动的原因"，而不是"必须有 Python"。
                assert!(
                    reason.contains("Python") || reason.contains("python"),
                    "不可用时的原因必须点出缺的是 Python，实际: {reason}"
                );
                println!("✓ 明确报不可用：{reason}");
            }
        }
    }

    #[test]
    fn interpreter_starts_runs_code_and_finalizes() {
        let _guard = serialize();
        if matches!(probe(), Availability::Unavailable { .. }) {
            println!("跳过：本机没有可用的 CPython");
            return;
        }
        let host = pinbridge_agent_test_host();
        println!("✓ 解释器就绪：{}", host.library());
        // 断言是**条件式**的，因为它本来就是条件性质：
        //   * 解释器是这个宿主初始化的 → 它**必须**交出 GIL，
        //     否则任何别的线程一调解释器就永久阻塞（实测过一次：整个测试进程挂住 10 分钟）；
        //   * 解释器是别人先初始化的（例如目标自己嵌了 Python、或同进程另一条用例先跑）
        //     → 我们不是主人，不能替别人的 GIL 状态背书。
        assert!(
            !host.owns_interpreter() || host.gil_was_released(),
            "自己初始化的解释器必须交出 GIL（owns={}, released={}）",
            host.owns_interpreter(),
            host.gil_was_released()
        );
        // 真正的执行：算一个数并让 CPython 自己检查结果。
        // 用 assert 而不是 print：脚本失败时 `run` 必须返回错误，
        // 而"打印成功"是看不出这一点的。
        host.run("assert 6 * 7 == 42, 'interpreted'")
            .expect("脚本应当成功");
        host.run("import sys; assert sys.version_info[0] == 3")
            .expect("3.x");
        let error = host
            .run("raise RuntimeError('boom')")
            .expect_err("未捕获异常必须报错");
        assert!(error.to_string().contains("脚本执行失败"), "实际: {error}");
        println!("✓ 异常路径也如实报错：{error}");
    }

    /// 单测里每条用例都要自己的宿主：`shared()` 是全局单例，
    /// 而解释器的初始化/收尾不适合被多条并发用例共享。
    fn pinbridge_agent_test_host() -> crate::python_host::imp::PythonHost {
        crate::python_host::imp::PythonHost::start().expect("已确认可用")
    }

    /// 端到端：注入 `pb` 模块 → 脚本 `subscribe` → 分发一个事件 →
    /// 脚本 `pb.mark` 把看到的数值报回来 → drain 读到它。
    ///
    /// 这条是"AI 写的脚本能在目标进程里运转"的最小闭环证据：
    /// Rust 侧的事件 → Python 回调 → Python 的输出 → Rust 读到。
    #[test]
    fn pb_module_round_trips_an_event_through_a_script() {
        let _guard = serialize();
        if matches!(probe(), Availability::Unavailable { .. }) {
            println!("跳过：本机没有可用的 CPython");
            return;
        }
        let host = pinbridge_agent_test_host();
        host.bootstrap().expect("注入 pb 模块应当成功");

        let script = r#"
import pb

def on_syscall(ev):
    pb.log("saw syscall %d" % ev.args[0])
    pb.mark("seen", ev.args[0])

pb.subscribe("syscall_enter", on_syscall)
pb.log("script loaded")
"#;
        host.run(script).expect("脚本应当成功执行");

        // 1) 脚本加载时的那条 log 已经在队列里。
        //
        // 队列项的编码是 `[kind, value, text]`，kind 用**数字**：
        //   1 = log（value 恒为 0）、2 = mark（value 是地址）、3 = subscribe（value 是订阅者数）。
        // 用数字而不是字符串，是为了让 Rust 侧不必解析文本就能分类；
        // 文本始终在第三项，需要时原样带走。
        let first = host.drain_outputs().expect("drain 应当成功");
        assert!(
            first.contains("[3, 1, \"syscall_enter\"]"),
            "应当有 subscribe 记录（kind=3，1 个订阅者）：{first}"
        );
        assert!(
            first.contains("script loaded"),
            "应当有加载时的 log：{first}"
        );
        println!("✓ 加载期输出：{first}");

        // 2) 分发一个事件：脚本应当被调用一次
        let payload = serde_json::json!({
            "address": 0x7ff6_0000_1000_u64,
            "args": [0x50, 0, 0, 0, 0, 0, 0, 0],
        })
        .to_string();
        let called = host
            .dispatch("syscall_enter", &payload)
            .expect("分发应当成功");
        assert_eq!(called, 1, "恰好一个订阅者应当被调用");

        // 3) 脚本用 pb.mark 把 SSN 报回来了 —— 证明它真的看到了事件内容
        let second = host.drain_outputs().expect("drain 应当成功");
        println!("✓ 分发后输出：{second}");
        assert!(
            second.contains("[2, 80, \"seen\"]"),
            "脚本应当用 mark（kind=2）把 args[0]（0x50 = 80）报回来：{second}"
        );
        assert!(
            second.contains("saw syscall 80"),
            "log 文本应当带上数值：{second}"
        );

        // 4) 没有订阅者的事件名：调用数必须是 0，而不是"装作分发过"
        let none = host
            .dispatch("mem_write", &payload)
            .expect("分发到无人订阅的事件名也应当成功");
        assert_eq!(none, 0, "没有订阅者时应当返回 0");
        println!("✓ 未订阅事件名返回 0（不假装分发过）");
    }
    /// 死循环脚本必须**在预算内被中断**，而且之后解释器仍然可用
    /// （"目标仍可继续"）。
    #[test]
    fn a_dead_loop_script_is_interrupted_within_its_budget() {
        let _guard = serialize();
        if matches!(probe(), Availability::Unavailable { .. }) {
            println!("跳过：本机没有可用的 CPython");
            return;
        }
        let host = pinbridge_agent_test_host();
        host.bootstrap().expect("注入 pb 模块应当成功");

        // 只有一行的死循环：这正是不用"调用级"跟踪而用**行级**跟踪的原因。
        let started = std::time::Instant::now();
        let outcome = host
            .run_with_budget("while True:\n    pass\n", 120)
            .expect("超时不是错误，是一种结果");
        let elapsed = started.elapsed();
        assert_eq!(outcome, RunOutcome::TimedOut, "必须报超时");
        // 预算 120 ms，允许调度与解释器启动的余量；关键是不能无限等下去。
        assert!(
            elapsed.as_millis() < 5_000,
            "应当在预算附近被中断，实际 {elapsed:?}"
        );
        println!("✓ 死循环被中断：{elapsed:?}");

        // 目标仍可继续：解释器还能正常跑别的脚本
        host.run("assert 2 + 2 == 4")
            .expect("中断之后解释器应当仍可用");
        println!("✓ 中断之后解释器仍可用");

        // 超时计数是一次性的：取走之后再跑正常脚本不该被当成超时
        let again = host
            .run_with_budget("value = 6 * 7\nassert value == 42\n", 5_000)
            .expect("正常脚本应当成功");
        assert_eq!(again, RunOutcome::Completed);
        println!("✓ 超时计数不会污染下一次执行");

        // 预算为 0 = 不限制：长一点的正常脚本不该被误杀
        let unlimited = host
            .run_with_budget("total = 0\nfor i in range(200000):\n    total += i\n", 0)
            .expect("不限制预算时应当成功");
        assert_eq!(unlimited, RunOutcome::Completed);
        println!("✓ 预算 0 = 不限制");
    }

    /// 分发路径的预算：回调里的死循环也要被中断。
    #[test]
    fn a_looping_callback_is_interrupted_too() {
        let _guard = serialize();
        if matches!(probe(), Availability::Unavailable { .. }) {
            println!("跳过：本机没有可用的 CPython");
            return;
        }
        let host = pinbridge_agent_test_host();
        host.bootstrap().expect("注入 pb 模块应当成功");
        // 第一次注册也要打标记：下面靠"同名标记"把这次留下的处理器清掉。
        host.set_script_tag("swapped").expect("打标记应当成功");
        host.run(
            r#"
import pb

def on_syscall(ev):
    while True:
        pass

pb.subscribe("syscall_enter", on_syscall)
"#,
        )
        .expect("注册脚本应当成功");

        let payload =
            serde_json::json!({ "address": 4096, "args": [1, 0, 0, 0, 0, 0, 0, 0] }).to_string();
        let outcome = host
            .dispatch_with_budget("syscall_enter", &payload, 120)
            .expect("超时不是错误");
        assert_eq!(
            outcome,
            DispatchOutcome::TimedOut,
            "回调里的死循环必须被中断"
        );
        println!("✓ 回调里的死循环被中断");

        // 中断之后仍然可用：换一个不循环的处理器。
        //
        // 这里必须先 `set_script_tag`：它清掉**同名脚本**留下的旧处理器。
        // 不清的话旧的那个死循环处理器仍在订阅表里，下一次分发还会超时——
        // 而且没有任何错误信息，只有"改了脚本却没生效"的结果。
        host.set_script_tag("swapped").expect("打标记应当成功");
        host.run(
            r#"
import pb

def on_syscall(ev):
    pb.log("ok")

pb.subscribe("syscall_enter", on_syscall)
"#,
        )
        .expect("重新注册应当成功");
        let outcome = host
            .dispatch_with_budget("syscall_enter", &payload, 2_000)
            .expect("分发应当成功");
        assert!(
            matches!(outcome, DispatchOutcome::Called(count) if count >= 1),
            "实际: {outcome:?}"
        );
        println!("✓ 中断之后分发仍然可用");
    }
    /// `pb.read` 端到端：脚本读一段**真实的目标内存**，把读到的魔数报回来。
    ///
    /// 地址通过 `__main__` 的一个全局变量交给脚本——这正是 AI 会遇到的形态：
    /// 地址来自别处（模块列表、事件、上一次读取），脚本拿它去读。
    #[test]
    fn pb_read_returns_real_target_memory() {
        let _guard = serialize();
        if matches!(probe(), Availability::Unavailable { .. }) {
            println!("跳过：本机没有可用的 CPython");
            return;
        }
        // 1) 准备一段真实内存（agent 自己的地址空间里）并写进魔数
        static mut TARGET_BUFFER: [u8; 16] = [0u8; 16];
        let address = std::ptr::addr_of_mut!(TARGET_BUFFER).cast::<u8>() as u64;
        crate::memory::write_process(address, b"PBREAD42").expect("写测试缓冲区应当成功");

        // 2) 给脚本一个能力集（含 mem_read），再加载脚本
        let capability_ok = crate::exports::pb_set_capabilities(
            pinbridge_core::capability::Capability::read_only_baseline().bits(),
        );
        assert_eq!(capability_ok, 0, "设置能力应当成功");

        let host = pinbridge_agent_test_host();
        host.bootstrap().expect("注入 pb 模块应当成功");
        // 地址作为全局注入：脚本从 globals 里拿
        host.run(&format!("PB_TARGET_ADDRESS = {address}"))
            .expect("注入地址应当成功");
        host.run(
            r#"
import pb

data = pb.read(PB_TARGET_ADDRESS, 8)
pb.mark("magic", int.from_bytes(data, "little"))
"#,
        )
        .expect("脚本读内存应当成功");

        let drained = host.drain_outputs().expect("drain 应当成功");
        println!("✓ 脚本读到的内存：{drained}");
        let expected: u64 = u64::from_le_bytes(*b"PBREAD42");
        assert!(
            drained.contains(&format!("[2, {expected},")),
            "脚本应当把读到的 8 字节按小端报回来（期望 {expected}）：{drained}"
        );
    }

    /// 没有 `mem_read` 能力时，`pb.read` 必须**抛异常**，而不是返回空数据。
    ///
    /// 返回空数据会让脚本把"没权限"当成"这段内存是零"——那是**静默的错误结论**，
    /// 比报错危险得多。
    #[test]
    fn pb_read_is_gated_by_capability_and_says_so() {
        let _guard = serialize();
        if matches!(probe(), Availability::Unavailable { .. }) {
            println!("跳过：本机没有可用的 CPython");
            return;
        }
        // 清空能力（含 mem_read）
        let cleared = crate::exports::pb_set_capabilities(0);
        assert_eq!(cleared, 0);

        let host = pinbridge_agent_test_host();
        host.bootstrap().expect("注入 pb 模块应当成功");
        let outcome = host.run(
            r#"
import pb

try:
    pb.read(0x1000, 4)
    raise AssertionError("没有 mem_read 却读成功了")
except RuntimeError as exc:
    pb.log("denied: %s" % exc)
"#,
        );
        // 脚本自己处理了异常，所以 run 应当成功；关键看它记录下来的原因。
        outcome.expect("脚本应当能捕获并记录拒绝原因");
        let drained = host.drain_outputs().expect("drain 应当成功");
        println!("✓ 能力拒绝被脚本看见：{drained}");
        assert!(
            drained.contains("denied"),
            "脚本应当记录下拒绝原因：{drained}"
        );
        assert!(
            drained.to_lowercase().contains("capability")
                || drained.contains("能力")
                || drained.contains("mem_read"),
            "拒绝原因必须点明是能力不足：{drained}"
        );
        // 恢复：别的用例还要用解释器
        let _ = crate::exports::pb_set_capabilities(
            pinbridge_core::capability::Capability::read_only_baseline().bits(),
        );
    }
    #[test]
    fn missing_library_is_an_actionable_error_not_a_fallback() {
        let _guard = serialize();
        // 显式指定一个不存在的库：必须**报错**而不是回退到别的版本——
        // 回退会让"我指定的库没生效"变成一个沉默的事实。
        let previous = std::env::var(DLL_ENV).ok();
        // SAFETY: 单测内设置环境变量；Rust 2024 把 set_var 标为 unsafe。
        unsafe { std::env::set_var(DLL_ENV, "pinbridge-no-such-python.dll") };
        let error = crate::python_host::imp::PythonHost::start().expect_err("应当报错");
        match previous {
            Some(value) => unsafe { std::env::set_var(DLL_ENV, value) },
            None => unsafe { std::env::remove_var(DLL_ENV) },
        }
        let text = error.to_string();
        assert!(
            text.contains("pinbridge-no-such-python.dll"),
            "要带上库名：{text}"
        );
        assert!(text.contains(DLL_ENV), "要提示可以用环境变量指定：{text}");
        println!("✓ 缺库时的错误可行动：{text}");
    }
}
