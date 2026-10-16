# 变更日志

本文件记录**对外可见**的变更（对使用者的行为有影响的一切）。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。
`.pbt` 格式版本（`FORMAT_VERSION`）与 C ABI 版本（`PB_ABI_VERSION`）
与包版本**独立演进**，任何一处变动都会单独标注。

## [Unreleased]

### 变更
- 收紧 Dependabot 策略：所有生态**显式忽略 major 升级**（major 属于决策，走 ADR），
  GitHub Actions 与 npm 各自分组、限制并发 PR 数。原因：首次推送后 Dependabot 在两分钟内
  开了 10 条大版本 PR（react 18→19、typescript 5→7、actions/checkout 4→7 等），
  全部需要人工评估，噪音会淹没真正要处理的事。

### 计划中（M1）
- `pinbridge.dll` 接通 Intel Pin：JIT 探针、上下文控制、TF 虚拟化。
- agent 内嵌 CPython 3.x：脚本加载/替换/卸载与执行时间预算。
- 真实内存读写、断点、单步、系统调用 Hook。

## [0.1.0] - 2026-10-16

M0：**工程骨架**。目标不是功能完备，而是"把规范、边界与验证方式先钉死"。

### 修复（M1：系统调用探针的崩溃——上一轮定位，这一轮解决）
- **回调里调用外部 DLL 会崩**（`RECEIVED_ACCESS_FAULT, Access Address = 0x8`，
  故障 IP 落在桥接层静态 CRT 区域内）。改成**回调只做内存入队 + Pin 内部线程
  每 10 ms 冲刷**之后：原来 5/5 必崩的命令现在 **6/6 通过**，轨迹记录 151 条 →
  448 条事件、零溢出丢弃。系统调用探针因此**默认开启**，并进 ctest
  （`pin_tool_captures_syscalls` 断言：捕够 300+ 条、溢出丢弃为 0、Pin 无内部故障）。
- **环形缓冲容量按实测数据定**：启动期的系统调用风暴在 10 ms 内能产生 400+ 条事件，
  128 条的环丢 39%（会报出来，但报出来不等于能接受）。改为 1024 条/线程 × 64 槽位。
- **`.pbt` 增量落盘**：收尾的 `stop_trace` 在 Pin 的 Fini 里，而 Fini **晚于**
  目标对 DLL 的卸载，桥接层的卸载保护会拒绝它（返回 `NOT_ATTACHED`）——
  实测表现为**轨迹文件 0 字节**。现在录制器每 64 条记录刷一次盘
  （`PbtWriter::flush`），退出期虽然拿不到 SUMMARY/END，磁盘上仍有可读记录，
  读取器如实报「汇总缺失」。这条对真实场景同样重要：被分析的程序崩溃或被强杀时，
  用户最需要的就是已经录到的东西。
- 工具侧把这次拒绝**说出来**（"退出期的收尾被卸载保护拒绝，这是已知取舍"），
  而不是只打一个 `-2`。

### 新增（M1：真实内存读写 + 脚本侧 `pb.read`）
- **内存读写不再是桩**。读走 `ReadProcessMemory`、写走 `WriteProcessMemory`，
  都在 agent 自己的进程里。**刻意不解引用**：agent 就在目标进程里，
  `*(addr as *const u8)` 对不可读地址是把目标打崩，而不是返回错误——而被分析的程序里
  到处是未映射页、`PAGE_NOACCESS` 保护页、只在某个瞬间可读的区域，
  "读之前不知道能不能读"是常态。系统调用会先做范围与保护校验，读不了就返回错误。
- **也不临时改保护属性**去"绕过"保护：那会改变目标的内存保护状态，
  在目标自己的线程与我们并发时留下一个真实的时间窗。读不到就说读不到。
- 三条行为约定：
  1. **部分写入按失败处理**——"我要写 100 字节、实际写了 40 却没告诉我"
     会让调用方以为整段都写进去了；
  2. 错误文本**带上地址与 Windows 错误码**，并把最常见的两个错误码翻成人话
     （`299` 部分映射 / `998` 保护属性不允许），因为它们直接决定下一步动作；
  3. 被拒绝的写**绝不落盘**（用例显式断言缓冲区仍是零）。
- **脚本侧 `pb.read(address, length) -> bytes`**：这是**反向调用**——脚本要求 agent
  做事，需要同步拿到结果。做法是 Rust 用 `PyCFunction_NewEx` + 一张**静态**
  `PyMethodDef` 造出真正的 Python 可调用对象。方法表必须是静态的：
  **CPython 只存指针、不拷贝**。只写 `PyMethodDef`（4 字段）不写 `PyModuleDef`（多一倍）。
- `pb.read` 的两条关键性质：
  1. **能力闸门在 Rust 侧**，且直接调用 agent 自己的 `pb_memory_read` 导出——
     Hub 侧与脚本侧**走同一个门**，不存在"脚本专用的小门"。
     实测：空能力集下脚本拿到 `RuntimeError: 能力不足: 需要 MemRead，当前持有 CapabilitySet(0)`。
  2. **拒绝时抛异常，不返回空数据**——返回空数据会让脚本把"没权限"当成
     "这段内存是零"，那是静默的错误结论，比报错危险得多。
- **回调路径用独立的函数表**（加载时再 `LoadLibraryA` 一次拿独立引用）：
  分发的加锁顺序是"先锁宿主 → 再取 GIL"，而回调是在**已持有 GIL**时被调用的；
  共用同一把锁就是死锁配方（A 持 GIL 等锁、B 持锁等 GIL）。
- `pb.read` 单次上限 4 MiB：脚本是"在里面"读，不用把数据搬过 socket，
  但没有上限的话一次手滑的 `pb.read(addr, 1<<40)` 会试图分配天文数字的缓冲区。
- 导出面新增两条用例：真实读写往返 + 不可读地址报错带地址；
  `link` 上"两个闸门"的用例扩成**三类**结果（能力不足 / 未实现 / 目标侧这次操作失败），
  因为内存读不再是"未实现"的例子——第二类改用断点。
- 修掉两处**过期期望**：`abi_surface_smoke` 里"读内存应当 not_supported"、
  `the_two_gates_are_distinguishable_on_the_link` 里拿 `ReadMemory` 当"未实现"的例子。
### 新增（M1：`pb` 模块 + 探针事件驱动脚本——M1 的核心能力通了）
- 脚本现在**真的能跑并收到事件**了。完整链路：
  `pb_load_script`（策略检查 → 注入 `pb` 模块 → 执行脚本 → 登记订阅）
  → `pb_push_event`（**先分发再入队**）→ 脚本回调 → `pb.log`/`pb.mark`
  → 输出转成 `script_output` 事件进事件流。
- `pb` 模块用**纯 Python 注入**，不写 C 扩展模块：`PyModuleDef`/`PyMethodDef`
  的结构体布局写错就是目标进程里的随机崩溃，而这一层需要的东西
  （收集输出、按事件名分发、payload 变成带属性的对象）用 Python 更短更清楚。
  Rust 只保留两个窄入口：`_pb_drain_json()` 与 `_pb_dispatch(combined_json)`。
- 扩展的 FFI 只用**不透明指针 + 非变参**函数（`python3.dll` 稳定 ABI 实测可用）：
  `PyImport_AddModule` / `PyObject_GetAttrString` / `PyObject_Call` /
  `PyTuple_New` / `PyTuple_SetItem` / `PyUnicode_FromString` /
  `PyUnicode_AsUTF8String` / `PyBytes_AsString` / `PyBytes_Size` / `PyObject_Str` /
  `Py_DecRef`。`PyObject_CallOneArg` 不在 ABI 里，所以走 `PyObject_Call` + 元组。
- 引用计数交给 **RAII 类型**（`Owned`）管：CPython 漏一次引用就是内存泄漏，
  多减一次就是随机崩溃，而手写 `DecRef` 要分散在十几条 early-return 路径上。
- 三条写进代码并被测试锁住的约定：
  1. **订阅列表以脚本的实际调用为准**（不解析源码文本）——`pb.subscribe` 可能在
     条件分支里、名字可能是拼出来的，文本解析会给出"看起来对但没生效"的列表，
     而分发侧只看列表，脚本会**静默地**永远收不到事件；
  2. **订阅未知事件名要报错**（`invalid_argument`），不能当没发生过；
  3. 事件名与 `EventKind::name()` 用**同一张表**（两处不一致 = "注册成功但永远
     不被调用"，那是最难查的一类问题）。
- **先分发再入队**：脚本在回调里的输出会紧跟"触发它的那条事件"出现在事件流里；
  反过来做，因果顺序在轨迹里看起来是反的。
- **脚本出错不让探针失败**：错误记进 `last_error`，`pb_push_event` 照常成功。
  一个写错的脚本不该把插装链路带崩。
- 结果转换补一步 `PyObject_Str`：脚本返回 `int`（例如订阅者数量）是正常用法，
  直接按字符串读会抛 TypeError，而错误文本会变成与真实原因无关的
  "返回值不是字符串"——这个坑在测试里撞到过一次。
- 新增导出面端到端用例：加载订阅 `syscall_enter` 的脚本 → 推一条事件
  （`args[0]=0x50`）→ 断言脚本把 `0x50` 用 `mark` 报回来并变成 `script_output`
  事件；另有一条用例覆盖"未订阅的事件名返回 0（不假装分发过）"。
  用例**循环取空**事件队列后再断言：队列是全进程共享的，
  只取一批会把本次输出挤到批次外，那是在测别的用例推了多少事件。
- `pb_load_script` 的成功路径现在返回 `Ok` 并说清用的是哪个解释器、脚本号是多少。
### 新增（M1：agent 内嵌 CPython——解释器宿主跑起来了）
- 设计文档决定"内嵌 CPython"，这一轮落地它的**前提部分**：解释器能在目标进程里
  定位、初始化、执行脚本、收尾。
- 装载方式是**运行时动态加载**（`LoadLibraryA` + `GetProcAddress`，按
  `python3.dll`（稳定 ABI）→ `python314.dll` → … 的顺序，支持 `PINBRIDGE_PYTHON_DLL`
  显式指定）。**不链接导入库**：那会把 agent 变成"依赖 python314.dll 才能加载"，
  于是没装 Python 的机器上连"读内存"这种无关能力也一起没了，而且报错发生在
  `LoadLibrary` 那一层，离原因很远。
- 三条约定写进代码并被测试锁住：
  1. **显式指定却加载失败不回退**（回退会让"我指定的库没生效"变成沉默的事实）；
  2. **不接管信号处理**（`Py_InitializeEx(0)`：目标进程的信号处理是它的）；
  3. **不是我们的解释器就不碰**（目标自己嵌了 Python 时只使用、绝不 finalize——
     拆掉目标的解释器会表现为"目标在某个时刻莫名崩了"）。
- **策略检查先于解释器**：违规脚本报 `invalid_argument`，错误文本里**不出现**
  "解释器"；干净脚本才走到解释器那一步。
- 实测（本机）：`probe()` 找到 `python314.dll`；单测在进程内跑
  `assert 6 * 7 == 42`、`import sys; assert sys.version_info[0] == 3`；
  未捕获异常 → 明确的 `Script` 错误；指定不存在的库 → 报错带上库名与
  `PINBRIDGE_PYTHON_DLL` 提示。
- `pb_load_script` 的返回文本现在精确区分三种结局：策略拒绝 / 解释器不可用（附原因）/
  解释器已就绪但 `pb` 模块与回调分发尚未实现。**不假装脚本已注册。**
### 修复（CPython 宿主里两个只有跑**整套**测试才会暴露的深坑）
- **解释器绝不 `Py_FinalizeEx`**。CPython 的解释器是**进程级**的：finalize 会把整个
  进程的解释器拆掉——包括**目标自己嵌的那一个**（被分析的程序完全可能自带 Python），
  而且会销毁 GIL 与全部解释器状态，此时别的线程/回调可能仍持有引用。
  实测表现：加上 finalize 之后，同一进程里的多条用例互相把解释器拆掉，
  整个测试进程直接 `STATUS_ACCESS_VIOLATION`。现在 `Drop` 只释放本结构体那次
  `LoadLibraryA` 的引用计数，不碰解释器；解释器活到进程结束——这正是我们要的，
  因为 agent 的生命周期就等于目标的剩余生命周期。
- **初始化线程必须把 GIL 交出去**（初始化后立刻 `PyEval_SaveThread`）。
  否则初始化线程一直持有 GIL，而任何**别的**线程一调 `PyGILState_Ensure` 就永久阻塞。
  实测表现：cargo 主线程初始化了解释器（于是持有 GIL）并在等测试线程，
  测试线程正在 `Ensure` 上等主线程——整个测试进程挂住 10 分钟。
  生产路径同理：探针回调跑在**目标线程**上，初始化线程绝不能霸着 GIL。
- 调用解释器时**成对持有 GIL**（`PyGILState_Ensure` / `PyGILState_Release`）：
  不在 GIL 保护下执行解释器代码是内存损坏，表现为随机访问违例（实测过一次）。
- 探测结果缓存并**静态持有**已加载的句柄：卸载一个可能正被使用的解释器是崩溃来源
  （实测：探测把 `python314.dll` 卸掉时另一个用例的解释器正活着 → 进程异常退出）。
  "不卸载"用静态的生命周期表达，而不是 `mem::forget`——仓库的 lint 表禁止后者，
  理由是句柄泄漏会表现为 Pin 侧无法 re-attach。
- 单测串行化（复用既有的 `GLOBAL_STATE_LOCK`）：CPython 不能在同一进程里并发
  初始化/收尾，而这几条用例还会改环境变量。
- 把"GIL 是否已交出"做成**可断言的性质**（`gil_was_released`）：断言是**条件式**的
  ——自己初始化的解释器必须交出 GIL；别人先初始化的（目标自带 Python、
  或同进程另一条用例先跑）我们不是主人，不能替别人的 GIL 状态背书。
### 新增（M1：`syscall_exit` 返回事件——`sysret` 那条路走不通，改为算返回地址）
- 第一版挂在 `INS_IsSysret` 上，实测一次都没命中（`exit=0`）。原因不是 Pin 的问题：
  内核返回用户态用的 `sysret` 是**内核态指令**，Pin 插装的是用户态代码；
  用户态能看到的返回路径就是 ntdll 桩里 `syscall` 的**下一条指令**。
- `INS_Next(syscall)` 也拿不到它：`syscall` 在 Pin 的模型里是**基本块终止指令**
  （实测 59/59 都"没有返回点"）。所以返回地址改为**算出**：
  `syscall 地址 + INS_Size`，登记进按地址排序的表，插桩期按地址认出来。
- 成立条件写进了注释：含 `syscall` 的块必然先被翻译执行，之后内核才返回到下一条
  指令，而那条指令所在的**新块**是那时才翻译的。少数反例（从别处跳进返回点）
  会让"登记数"与"成功插装数"对不上，收尾统计把这笔差额**如实报出来**。
- **没有配对进入的返回记为 `args[0] = 0` 并计数**：ntdll 桩的 `int 2Eh` 回退路径与
  `syscall` 路径**共用同一个 `ret`**，那条 `ret` 两条路都会经过。此时拿上一次的
  调用号填 `args[0]` 就是伪造数据——宁可记 0（未知）也不猜。
- 实测：441 `syscall_enter` / 454 `syscall_exit`；轨迹里 `args` 是 `(14, 0)`、`(80, 0)`
  这种真实的「调用号 + NTSTATUS」；**16 条 `args[0] == 0` 与统计里 16 次未配对返回
  完全吻合**（这是判据，不是巧合）。
- 判据进 ctest：`pin_tool_captures_syscalls` 要求进入与返回**都** ≥300
  （新增 `-pb_require_syscall_exits`），并把 `exit=0` 直接列为失败条件——
  那条正则正是第一版缺陷的形状。
### 新增（M1：`pb_modules`——目标进程内的模块枚举）
- agent 侧新增 `modules` 模块：走 **PEB 加载器链**
  （`gs:[0x60]` → `PEB->Ldr`(+0x18) → `InLoadOrderModuleList`(+0x10)）做进程内枚举。
  选它而不是 `EnumProcessModules`：agent 本来就活在目标进程里，走 PEB 是纯内存读，
  不需要打开句柄、不引入 psapi 依赖。
- **快照语义如实上报**：读加载器链不持 loader lock，所以它是一个瞬间快照
  ——链正在被修改时会出现半初始化条目。这些条目**跳过并计数**（`skipped`），
  一路带到 MCP 工具的输出里。一个"看起来完整"的列表比一个"标了可能不完整"的列表危险得多。
- 协议新增 `AgentRequest::Modules` / `AgentResponse::Modules` / `ModuleInfo`
  （名字、基址、映像大小、入口点——只放换算地址需要的四项）；能力档位 `mem_read`。
- Hub 新增 `TargetLink::list_modules` 与 `HubService::list_modules`（校验顺序与
  `read_memory` 一致）；MCP 的 `ClientLink` 实现适配；`pb_modules` 工具接通，
  输出基址 + 大小 + 完整路径，并在 `skipped != 0` 时明确提示列表可能不完整。
- 实测：agent 自己的测试进程里枚举到 **9 个模块**，主模块在首位、路径完整、跳过 0。

- **非 Windows 的那一半契约也要测**：`modules_request_enumerates_this_process` 第一版
  把「非 Windows 应当是明确的 not_supported」写在了 `Modules` 分支**里面**，
  于是它在 Linux 上根本不会执行——CI 直接以
  `期望 Modules，实际: Error { not_supported }` 失败。现在两个平台的期望各占一个
  分支：Windows 断言"枚举到 ≥3 个模块且路径完整"，其它平台断言"明确拒绝并说清原因是 PEB"。
  这正是"本地跑不了的平台要靠 CI 兜住"的一个实例：本地门禁只做 Linux 的 **clippy**，
  跑不了 Linux 的测试。
### 修复
- **`UNICODE_STRING` 的 `Buffer` 偏移是 8，不是 4**（`{Length, MaximumLength,
  <4 字节填充>, Buffer}`）。第一版按 `2 * size_of::<u16>()` 读，读到的是填充区，
  于是拿着垃圾指针解引用——测试直接 `STATUS_ACCESS_VIOLATION`。
  这类错误只有**真的去读一次 PEB** 才会暴露，写在注释里了。
- `only_script_load_executes_in_target` 这条断言随 `pb_modules` 落地而更新：
  现在有**两个**工具的工作过程发生在目标内，且理由不同（一个读目标的加载器链、
  一个在目标内执行脚本）。集合仍然必须小而明确，所以测试改名成了
  `only_deliberate_tools_execute_in_target` 并把两条理由写在注释里。
- 修一处 `Modules` 变体插入时把 `ReadRegisters` 的文档注释「抢走」的排版问题
  （`missing documentation` 警告暴露了它）。

### 新增（M1：控制面到目标端的主动请求链路——`pb_memory_read` 打通）
- Hub 增加 [`TargetLink`] trait：Hub 只声明"我需要读目标内存"，具体是 socket、
  命名管道还是假的实现由上层决定。**这是让 Hub 的闸门逻辑可测的关键**——
  否则每个 Hub 测试都要一个能通话的对端。
- 会话条目增加可选链路，新增 `attach_link`（需要 `ProcessControl`：挂链路会扩大
  这个会话能做的事）与 `read_memory`。校验顺序刻意是
  **参数 → 能力 → 会话状态 → 链路**：一个 `length=0` 的请求任何状态下都是调用方的
  bug，不该被翻译成"目标不可用"——那会让人去查目标。
- 没挂链路时的错误**点明缺的是链路**（含"主动请求需要先 attach_link"），
  而不是含糊的"目标不可用"。
- 组合根（MCP）提供 `ClientLink<T>`：把 `pinbridge_client::AgentClient` 适配成
  `TargetLink`。放在 MCP 而不是 Hub/client 是有意的——Hub 依赖 socket 层、
  client 反向依赖 Hub，两个方向都是错的。
- `pb_memory_read` 接通：返回十六进制切片 + 起始地址，并说明回读长度与请求一致。
- 测试：适配器层（hex 解码、长度不符、agent 错误翻译、对端回错类型、奇数长度 hex、
  空客户端能力集被本地拦下）、Hub 层（三重闸门、参数校验、长度不符）、
  MCP 层（没挂链路时的错误 → 挂上链路后真的读出 `90cc4889`）。

### 修复
- `.pbt` **长度不符必须报错**这条规则现在有两处实现：适配器（给出"agent 回了 N 字节、
  我要 M 字节"的链路层信息）与 Hub（语义层最终把关）。两处的错误信息面向的排查方向
  不同，所以不是冗余。
- 记下第三个能力闸门：`AgentClient` **自己也有一道**（默认空集）。不给它能力集时
  每个请求都会在客户端被拒成"当前持有 CapabilitySet(0)"——看到这句话就该想到是
  适配器没带能力集，而不是目标不支持。整条链其实是 **client → hub → agent** 三重闸门。

### 新增（M1：Hub 侧加载器打通"目标内录制"）
- `pinbridge-sys` 的 `Api` 增加 `start_trace(path, fold)` / `stop_trace() -> (u64, u64)`，
  并在加载时解析 `pb_start_trace`/`pb_stop_trace` 两个符号。意义：轨迹由**目标那一侧**
  写盘，而不是把事件全量搬回 Hub 再拼；少了这两个符号，"录一份轨迹"就只剩
  "Hub 自己收事件"这一条路。
- 参数校验先于状态判断（与 `drain` 的 0 容量同一条原则）：路径含 NUL 直接
  `Protocol` 错误，**不允许截断成半条路径**。
- 端到端测试（`crates/pinbridge-sys/tests/bridge_end_to_end.rs`）真的加载 DLL、
  attach、下发能力，然后**录出一份 186 字节、`PBT1` 魔数正确的 `.pbt`** 并取回计数；
  同时断言路径含 NUL 被拒。这条路径之前只被"缺少 DLL 时给出明确错误"覆盖过。

### 修复
- MCP `pb_memory_read` 的返回文本与现实脱节：它说"M0 阶段依赖 agent 链路（Pin），
  尚未接通"，但实际上 agent 侧 `AgentRequest::ReadMemory` 已是真实实现（含单次读取
  上限与明确拒绝）、`pinbridge-client` 也能和 agent 通话。真正的缺口是
  **Hub 的会话条目里只持有 `session`/`runtime`/`scripts`，没有到 agent 的 client 句柄**。
  错误文本已改成如实描述这三段现状并指出下一步——含糊的"尚未接通"会让人去查错的
  那一端。

### 新增（M1：TF 单步虚拟化——第四类插桩点，四类至此全部落地）
- `-pb_tf off|watch|virtualize`：`watch`（默认）只观察目标对 EFLAGS.TF 的操作意图
  与真实单步异常并上报 `TrapFlag` 事件；`virtualize` 在 `popfq`/`iretq` 的**栈上
  flags 副本**里剥掉真实 TF，并在 `pushfq` 的 `IPOINT_AFTER` 把虚拟 TF 补回读副本。
- 为什么改栈上的副本：`popfq`/`iretq` 会从栈加载 flags，在 `IPOINT_BEFORE` 改 EFLAGS
  会被这条指令自己覆盖；读的那一侧相反，必须在 AFTER 改刚压好的那份。
- 实测四档签名（每一档都可证伪，已进 ctest）：
  * 单独跑靶子：`real_steps=50 tf_reads=0`（内核派发异常时清掉了 TF）；
  * Pin + `off`/`watch`：进程以 `0x80000004`（STATUS_SINGLE_STEP）终止；
  * Pin + `virtualize`：**`real_steps=0 tf_reads=50`** —— 目标确信自己在单步，
    其实一次都没被单步；工具侧同时看到置位意图 50、剥离 50、读补 100、真实单步 0。

### 修复（TF 相关的两个"跑起来才发现"的坑）
- **`INS_Mnemonic` 返回大写助记符**（实测第一条指令打印出来是 `"MOV"`）。第一版按
  小写 `"popfq"` 比较，于是一个相关指令都没插桩，表现出来却是"虚拟化没生效"
  ——识别环节就断了。已在代码注释里写明。
- **在 Pin 下目标收不到自己的单步异常**：靶子自己的 SEH 抓不到 STATUS_SINGLE_STEP，
  进程直接以 `0x80000004` 退出，连 Pin 的 Fini 都不跑（所以"收尾统计"永远不会出现）。
  这正是虚拟化对本项目**必需**的理由：不虚拟化，任何自己开 TF 的目标都会死在 Pin 下。
  这条被写成一条刻意断言"会失败"的用例（`pin_tool_tf_without_virtualize_is_fatal`）：
  哪天它不再失败，说明 Pin 行为变了或我们不小心虚拟化了。

### 新增（M1：指令模板匹配）
- `-pb_pattern "id:name:hex"`（多条用 `;` 分隔，`?` 通配）：扫描主模块**可执行段**
  的字节签名，命中点插桩；执行到就打 `pattern_hit` 事件（`args[0]` = 模板 id）。
  这是"没有符号时怎么定位函数"的那条路。
- 每个模板的命中地址数有上限（`-pb_pattern_max`），**被截断的条数会报出来**；
  任何一条模板解析失败就拒绝继续——静默跳过某条会让"它没生效"看起来像"它没命中"。
- 靶子新增 `--pattern-hits N`：一个带 8 字节魔数的 `noinline` 函数，编译后魔数作为
  立即数出现在代码段里，因此可以被签名找到。
- 实测：登记命中点 1 个、靶子调用 500 次 → **命中 500 次**（精确相等），零丢弃；
  轨迹 `142 条记录 → 640 条事件`，类型分布含 `pattern_hit`。
  `ctest -R pin_tool_pattern_hit` 断言"调 500 次必须命中 500 次"。

### 修复（模板匹配的两个"跑起来才发现"的坑）
- **字节序**：立即数在机器码里是小端。靶子的 `0x5042524944474531` 在代码段里是
  `31 45 47 44 49 52 42 50`；按文本顺序写模式会命中 **0** 处，而且看起来像
  "模板功能没生效"而不是"模式写反了"。已在靶子注释与 README 里写明排查手段
  （直接在 exe 里按字节搜）。
- **命中点落在指令中间**：字节模式常常匹配到指令内部（`movabs` 的立即数偏移 +2）。
  第一版按"指令起始地址 == 命中地址"比对，于是扫描找到 1 个命中点、命中次数却是 0。
  现在按**指令地址范围** `[起始, 起始 + 长度)` 匹配。

### 新增（M1：内存访问探针）
- 内存探针：三层过滤（**模块**：默认只主模块；**地址范围**：`-pb_mem_range lo-hi`；
  **类型**：`-pb_mem 0|w|r|rw`）。写入事件用两段式拿到**写入后**的值
  ——`IARG_MEMORYWRITE_EA` 只在 `IPOINT_BEFORE` 有效，而那里读到的是写入**前**
  的值；不支持 AFTER 点的写入指令跳过并计数（宁可少采一条，也不送一条语义
  错误的"新值"）。
- 靶子新增 `--touch N`（对模块内固定缓冲区做 N 次读写）与 `--print-buffer`
  （打出缓冲区地址，供脚本构造地址范围）。
- 脚本化用例 `scripts/check-mem-probe.ps1`（ctest `pin_tool_mem_range_filter`）：
  先问靶子要地址，再带范围跑探针，三条判定一起看——写入数**精确相等**、
  溢出丢弃为 0、地址过滤计数 > 0。只有第一条会被"根本没过滤"骗过，
  只有第三条会被"什么都没捕到"骗过。
- 实测：靶子写 2000 次 → 探针捕获 `write=2000`、`read=0`、过滤掉 11252 条、
  零丢弃；从轨迹读出的值序列是 `0,1,2,3,4,5…`（正是靶子写的 `i & 0xFF`）。

### 修复
- **地址范围过滤形同虚设**：`-pb_mem_range` 只在插桩期判了模块，
  而插桩期不可能知道指令会访哪个地址——范围外的事件照样灌进来，
  环形缓冲被冲爆、丢掉一半（实测：送出 6722 条、丢弃 6831 条）。
  现在过滤在**回调里**做（一次比较），并计入"过滤掉 N 条"。
- 冲刷线程从"固定 10 ms 轮询"改成"有活就不睡、没活才睡 1 ms"：
  固定间隔在访存这种密集探针下会成为瓶颈（大部分时间花在睡眠上）。
- 环形缓冲 1024 → 2048 条/线程（线程槽位 64 → 32）：实测一个 2000 次的写循环
  会在一瞬间产生上千条事件，1024 条在突发下会丢。
- **内存探针默认关闭**：它在默认配置下（只采写入）启动阶段就能产生上千条事件
  并丢掉一半——"打开着但丢一半数据"比不打开更危险，因为使用者会以为看到了全部。

### 新增（M1：系统调用探针的插装与记账，**默认关闭**）
- Pin tool 增加系统调用探针：识别 `syscall` / `sysret` 指令（实测 59–60 个插装点），
  采集 SSN 与前四个参数（x64 的第一个参数在 **R10**，不是 RCX），
  带 `-pb_syscall_max` 上限、重入保护、以及"重入跳过/推事件失败/槽位溢出"
  三项记账（不允许静默丢事件）。
- 自写测试靶子扩成三档：默认（入口探针）、`--syscalls N`（N 次真实系统调用）、
  `--spin N`（**纯用户态、零系统调用**，作为反向对照：如果它也能产出一堆系统调用
  事件，那"捕获到事件"就不再是证据）。
- 构建工作流：`-WithPin` 改用**独立构建目录**（`native/build-pin`）。起因是一次真实
  的时间浪费——`verify.ps1` 会把 `native/build` 重新配置成不带 Pin，于是随后的
  "带 Pin 构建"其实什么都没编译，跑的还是旧 DLL（表现为旋钮"unknown option"）。

### 修复（这一轮真正的收获：三个"跑起来才暴露"的缺陷）
- **在系统调用回调里调用外部 DLL 会崩**（`RECEIVED_ACCESS_FAULT,
  Access Address = 0x8`，故障 IP 在桥接层静态 CRT 区域内）。定位到根因并给出
  可复现判据：回调零外部调用 → 3/3 正常；恢复调用 → 2/2 崩溃。因此系统调用探针
  **默认关闭**，修法（回调只入队 + Pin 内部线程冲刷）记入路线图。
- **桥接层的错误缓冲用了 `thread_local`**：本层用静态 CRT（/MT），DLL 卸载后
  线程私有数据被释放，之后访问 `thread_local` 就是"TlsGetValue 返回 null 再取
  偏移 8"。改成普通静态量；同时把"卸载中不触碰 CRT"写进转发前置检查。
- **桥接层漏了两个 tool ABI 导出**（`pb_start_trace`/`pb_stop_trace`）：
  头文件声明了、agent 实现了、桥接层没转发，于是"让目标自己录一份轨迹"
  这条路径在桥接拓扑下不可达，而失败信息只说"导出面不完整"。
- **轨迹元数据键名 `abel_version` 是错别字**（应为 `abi_version`）：不会立刻报错，
  只会让读取方永远拿不到 ABI 版本。加回归测试。
- 把"自己模块"排除出插桩范围：桥接层与 agent 是 `LoadLibraryA` 装进**目标进程**的，
  Pin 会把它们当成目标的一部分一起插桩，于是我们自己的系统调用会从我们自己的
  回调里再触发探针。

### 实测澄清（写进 ADR 0010 与 third_party/pin/README）
- AMD（Ryzen 7 9700X）上 Pin 3.31 的启动/插桩/事件回传链路**是通的**；
- `RTN_InsertCall(IPOINT_AFTER)`/`RTN_FindByAddress` 等需要 `PIN_StartProgramProbed()`；
- `INS_IsSysret` 在当前 Windows 目标上没匹配到（`exit=0`）。

### 新增（M0.5：Hub ↔ agent 回环链路）
- `pinbridge-agent/src/link.rs`：目标侧的**回环链路服务端**。它不自己判断权限，
  而是转发给 `exports::pb_*`——两条入口（C ABI 与回环链路）因此不可能对
  "该不该放行"产生分歧。只监听 `127.0.0.1`（没有地址参数，不是默认值而是设计）。
- `pinbridge-proto/src/agent_link.rs`：`AgentRequest`/`AgentResponse`/`EventDto`
  **以及那张能力表**从 client 移到 proto，让连接的两端用同一份定义；
  新增请求类型若不登记能力会被测试拦下。
- `link.rs` 的健壮性：单帧上限、超大帧拒绝（不按声明长度分配内存）、
  非法 JSON 回结构化错误、空闲超时（对端消失不会让服务端线程永久阻塞）、
  50 行一条注释说明"`from` 只做向前过滤"。
- `crates/pinbridge-client/tests/agent_link_end_to_end.rs`：真 socket 握手、
  版本协商、**双端闸门可区分**（客户端拦下的请求根本不到服务端，服务端只收到 5 条）、
  事件过链路逐字段完好。
- `pb_clear_breakpoint` 追加导出（只有"下"没有"清"会让使用者无法收回断点）。

### 新增（M0.5：C++ 桥接层）
- `native/src/agent_bridge.cpp` → `pinbridge_bridge.dll`：导出 Hub 期望的 8 个
  tool 符号并转发给真实 agent DLL。**不需要 Intel Pin**，因此"Hub ↔ agent"
  这条链路从"只测过类型"变成了"真实跑过"。
- `native/tests/bridge_roundtrip_test.cpp`：CTest 用例，含正向（真实 agent、
  能力闸门、事件跨边界往返）与反向（agent 缺失必须明确失败）两类。
- `crates/pinbridge-sys/tests/bridge_end_to_end.rs`：Hub 侧加载器端到端
  （真实 `LoadLibraryA` → ABI 校验 → 能力下发 → 事件取回 → 错误文本）。
  `PINBRIDGE_REQUIRE_NATIVE=1` 时**不允许跳过**（CI 的 native job 会设它）。
- `scripts/build-native.ps1`：一条命令完成 VS/vcvars/Ninja/CMake 发现、
  配置、构建与 CTest；本地与 CI 走同一条路径。
- `native/README.md`：这一层的构建方式、测试矩阵与"踩过的坑"。
- CI 新增 `native` job（windows-latest）：构建 agent + native 层 + CTest +
  加载器端到端测试。
- `scripts/verify.ps1` 新增 native 步骤（装了 VS 就自动跑）。

### 新增（M1：Pin 探针层落地，第一次真正插装一个进程）
- Pin 3.31 kit 就位并记录（版本串、zip 的 sha256、**实测**目录结构）。
  `scripts/fetch-pin.ps1` 的检查路径按实测布局修正——早先按记忆写成
  `source/tools/Pin/pin.H`（那是源码树内部的路径），结果脚本永远报"缺 Pin"。
- `native/src/pinbridge_tool.cpp`：Pin tool 骨架 + **入口点探针**。
  它只做三件事（注册探针、把事实推给 agent、开始/结束录制），
  依赖链是"探针 → 桥接层 → agent"——**复用**已测过的转发层，不重写一套。
  链路实测：`pin.exe -t pinbridge.dll -- pb_sample_target.exe`
  → 入口点命中 → 事件穿过两个语言边界 → **目标进程内写出 `.pbt``** →
  我们自己的读取器解析通过（`ctest -R pin_tool_instruments_a_target`）。
- `native/tests/sample_target.cpp`：自写的最小靶子。测试靶子一律自写，
  不拿系统二进制或外部样本——否则"测试失败"会同时包含"我们的工具坏了"
  与"那个二进制有反制"，两者必须能分开归因。
- `crates/pinbridge-agent/src/script.rs`：脚本**策略与生命周期**（M1 的执行层
  还没做，但登记、白名单、状态机、预算记账都能测了）。静态扫描给出**行号与理由**；
  文档与代码里都写明**它是策略过滤器而不是沙箱**（构造性绕过是可能的）。
  顺带把"生命周期上还相关（`is_live`）"与"该不该收新事件（`receives_events`）"
  拆成两个判断：混用会让被替换掉的旧脚本继续按老逻辑打日志。
- **ADR 0010**：探针层用 Pin，但当作可替换件。含 AMD 上的实测结论
  （Ryzen 7 9700X 上链路通）与备选（DynamoRIO/QBDI/TinyInst）。
- `scripts/build-native.ps1 -WithPin`、`native/CMakeLists.txt` 的 Pin 目标
  （参数逐条对应 kit 的官方 makefile，并注明行号）。

### 修复
- **桥接层漏了两个 tool ABI 导出**：`pb_start_trace`/`pb_stop_trace` 头文件声明了、
  agent 实现了，桥接层没转发——于是"让目标自己录一份轨迹"在桥接拓扑下不可达，
  而失败信息只是"导出面不完整"，看不出缺的是哪两个。这是 Pin tool 第一次跑起来
  才暴露的（Hub 侧加载器同样还没解析这两个符号，已记入路线图）。
- 轨迹元数据里的键名 `abel_version` 是错别字（应为 `abi_version`）。它不会被任何
  东西立刻报错，只会让读取方永远拿不到 ABI 版本；加了回归测试防止复发。
- `pinbridge-agent` 的桩实现状态码与错误文本不一致：内部先用 `Error::Trace` 走通用
  映射、再把码重映射成 `not_supported`，于是 `pb_last_error()` 里留着
  "内部错误: 轨迹错误: …"。现在 `stub()` 直接以 `NotSupported` 写文本，
  并删掉了 `mapped()` 这个重映射补丁。能力不足的文本也不再出现
  "能力不足: 能力不足:" 这种重复前缀。
- 链接路测试的偶发失败（"推 3 条取回 8 条"）：测试夹具的队列清理只排一次 64 条，
  上一个用例留下的多余事件会混进断言。现在循环拉空到 0，
  并把这条坑写进了夹具的注释（复位必须"干净"，不是"差不多干净"）。
- `pinbridge-sys` 在能力不足时把 `needed` 固定填成 `MemRead`：AI 会读到
  "需要 mem_read"并去申请一个它其实已经有的能力。`to_error`/`check` 现在
  要求调用方传入真实需要的能力；`Api` 记住下发过的能力集，`held` 如实回报。
  （`Api` 因此不再是 `Copy`——它持有状态，静默复制会让两份记录分叉。）
- `Status::NotSupported` 之前映射成 `Error::Trace`，会把读日志的人引向轨迹文件；
  现在映射为 `TargetUnavailable` 并带上"哪一层没实现"的说明。
- C++ 层首次真实编译暴露的三个缺陷：头文件缺 `<stddef.h>`（`offsetof` 找不到）、
  MSVC 未加 `/utf-8`（UTF-8 中文源码被按 GBK 解释，报出"字符串字面量中的换行符"
  这种与根因无关的错误）、`add_test` 用裸目标名导致 ctest 找不到可执行文件。
- MSVC 非英文界面下 `showIncludes` 前缀不匹配导致构建日志被刷屏（真正的错误被淹），
  现已显式设置 `CMAKE_CL_SHOWINCLUDES_PREFIX`。
- Python 侧首次通过 `ruff check` / `ruff format --check` / `mypy --strict`
  （本次 23 处 ruff 问题、5 处类型问题）：主要是行宽、`UP035` 导入来源、
  `UP037` 冗余引号，以及解码器参数类型（`bytes` → `bytes | bytearray`）。
  类型层面把"读取器到底依赖了什么"显式化成两方法的 `ReadableSource` Protocol，
  而不是假装满足 `BinaryIO`。
- `scripts/verify.ps1` / `verify.sh` 的 Python 侧从"只跑 pytest"补成
  pytest + ruff check + ruff format + mypy——这一步之前只在 CI 上跑，
  导致本地全绿而 CI 变红。
- `pinbridge-sys` 的 `PB_ABI_VERSION` 无条件导入：Windows 本地 clippy 全绿，
  Ubuntu 上因 `unused_imports` 直接失败。改为 `#[cfg(windows)]` 条件导入，
  测试模块显式导入；并把「跨平台 lint」加进 `scripts/verify.ps1` /
  `verify.sh` 与 `docs/dev/coding-standards-rust.md`。
- `commit-msg` 钩子的标题长度按字节计算导致合法中文标题被误判为超长；
  改为按 UTF-8 结构计算显示列（中文 2 列），不依赖 locale。

### 新增
- Rust 工作区（9 个 crate）：`core` / `proto` / `trace` / `hub` /
  `client` / `sys` / `agent` / `mcp` / `cli`，依赖方向单向。
- `.pbt` 轨迹格式 **v1**：chunk 容器 + delta/varint/zigzag 记录编码 +
  重复折叠标记；Rust 与 Python **双实现**共享同一组黄金字节用例。
- 重复折叠压缩：序号连续 + 载荷相同才折叠；压缩**可逆**（有测试）。
- 控制权模型：`Principal`（人与 AI 平级）、`CapabilitySet`（只减不增）、
  `ControlLease`（epoch 失效在途命令）。
- 会话状态机、有界事件通道（丢弃计数必须上报）、脚本生命周期与 sha256 溯源。
- MCP 服务：`initialize` / `tools/list` / `tools/call`，工具表按当前能力过滤，
  错误里带 `hint` 告诉 AI 下一步怎么办。
- CLI：`tools` / `replay` / `demo` / `mcp` / `version`，全部**离线可跑**。
- Python 侧：`.pbt` 解码、重放、结构化污点传播、OEP 候选定位、`pb-replay` 命令。
- 文档：架构/设计/规范/开发流程/ADR，共 20+ 篇，含对原文架构的逐条对照。
- 工程化：`rust-toolchain.toml` 钉版本、clippy pedantic 门禁、`deny.toml`
  供应链策略、CI（格式/lint/测试/端到端/Python/审计）、Issue 与 PR 模板。
- 端到端自检 `pinbridge demo`：建会话 → 拒绝越权 → 授权 → 装脚本 → 录轨迹 →
  折叠查询 → 读回校验 → 打印溯源。
- `NOTICE.md`：使用边界与第三方（Intel Pin）声明。`LICENSE` 保持纯 MIT 文本，
  便于 GitHub 自动识别许可证类型。

### 已知限制
- 所有需要 Pin 运行时支撑的动作是**桩实现**，显式返回 `not_supported`。
- agent 内嵌 CPython 尚未接通；`pb_load_script` 只做校验与报错。
- Hub 与 agent 之间的回环 TCP 链路已定义并实现传输抽象，真实握手见 M1。
- 桌面工作台（`apps/desktop`）只有骨架，未接真实数据。

### 不兼容变更
无（首个版本）。

<!--
提交 PR 时请顺手在 [Unreleased] 下加一条。CI 会检查含代码变更的 PR 是否
动过本文件（只动 docs/ 的 PR 可跳过）。
-->
