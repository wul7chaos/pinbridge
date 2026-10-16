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
    }

    // SAFETY: 这些指针由 DLL 持有；它们本身不访问线程局部状态。
    // 解释器的线程约束由上层（GIL 与"单线程调用"约定）保证。
    unsafe impl Send for PyApi {}
    unsafe impl Sync for PyApi {}

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
import json as _json, sys as _sys, types as _types

_PB_OUTBOX = []
_PB_SUBSCRIBERS = {}

def _pb_emit(kind, value, text):
    _PB_OUTBOX.append([kind, int(value), str(text)])

def _pb_log(text):
    _pb_emit(1, 0, text)

def _pb_mark(name, address=0):
    _pb_emit(2, int(address), str(name))

def _pb_subscribe(event_name, handler=None):
    name = str(event_name)
    handlers = _PB_SUBSCRIBERS.setdefault(name, [])
    if handler is not None:
        handlers.append(handler)
    _pb_emit(3, len(handlers), name)

_pb = _types.ModuleType("pb")
_pb.log = _pb_log
_pb.mark = _pb_mark
_pb.subscribe = _pb_subscribe
_pb.__doc__ = "PinBridge 目标侧接口（由 agent 注入，不是安装到环境里的包）"
_sys.modules["pb"] = _pb

def _pb_drain_json():
    payload = _json.dumps(_PB_OUTBOX)
    del _PB_OUTBOX[:]
    return payload

def _pb_dispatch(combined_json):
    combined = _json.loads(combined_json)
    name = str(combined.get("event"))
    handlers = _PB_SUBSCRIBERS.get(name, [])
    if not handlers:
        return 0
    event = _types.SimpleNamespace(**combined.get("payload") or {})
    for handler in handlers:
        handler(event)
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
            self.run(BOOTSTRAP)
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

    /// 非 Windows 平台的统一原因文本。
    const NOT_SUPPORTED: &str = "当前平台未实现 CPython 动态加载（M1 只支持 Windows）";
}

pub use imp::{bootstrap, dispatch, drain_outputs, library_description, probe, run_source};

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
