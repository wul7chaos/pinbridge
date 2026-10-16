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
