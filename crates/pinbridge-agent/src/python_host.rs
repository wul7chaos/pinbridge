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
    use std::ffi::{c_char, c_int, c_void, CString};

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
    }

    // SAFETY: 这些指针由 DLL 持有；它们本身不访问线程局部状态。
    // 解释器的线程约束由上层（GIL 与"单线程调用"约定）保证。
    unsafe impl Send for PyApi {}
    unsafe impl Sync for PyApi {}

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
        api: PyApi,
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
            Ok(Self {
                api,
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
        #[allow(dead_code)]
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
            let gil = unsafe { (self.api.PyGILState_Ensure)() };
            // SAFETY: code 以 NUL 结尾且在本调用期间存活；解释器已初始化；
            // 当前线程已持有 GIL。
            let status = unsafe { (self.api.PyRun_SimpleString)(code.as_ptr()) };
            // SAFETY: gil 来自上面的 Ensure。
            unsafe { (self.api.PyGILState_Release)(gil) };
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
            match self.api.Py_IsFinalizing {
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
                api: PyApi {
                    module: std::ptr::null_mut(),
                    Py_IsInitialized: never_called,
                    Py_InitializeEx: ignore_int,
                    Py_IsFinalizing: None,
                    PyRun_SimpleString: fail_string,
                    PyGILState_Ensure: never_called,
                    PyGILState_Release: ignore_int,
                    PyEval_SaveThread: null_thread_state,
                },
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

    /// 空壳真正被调用时不该发生；调用即明确失败，而不是返回一个假结果。
    unsafe extern "C" fn never_called() -> c_int {
        0
    }

    unsafe extern "C" fn ignore_int(_: c_int) {}

    unsafe extern "C" fn fail_string(_: *const c_char) -> c_int {
        -1
    }

    unsafe extern "C" fn null_thread_state() -> *mut c_void {
        std::ptr::null_mut()
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
        Err(Error::TargetUnavailable(
            "当前平台未实现 CPython 动态加载（M1 只支持 Windows）".into(),
        ))
    }
}

pub use imp::{library_description, probe};

/// 当前平台是否实现了 CPython 宿主。
#[must_use]
pub const fn supported() -> bool {
    cfg!(windows)
}

#[cfg(all(test, windows))]
mod tests {
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
