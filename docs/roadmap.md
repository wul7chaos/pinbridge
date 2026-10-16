# 路线图

每个里程碑的验收标准都是**可执行的**（一条命令、一个断言），
不是"功能开发完成"。

## M0 — 工程骨架与规范（✅ 已完成，2026-10-16）

**目标**：把规范、边界与验证方式先钉死。功能不完备没关系，
但"哪些是约定、哪些是桩、桩怎么失败"必须一眼可见。

| 交付 | 验收 |
|------|------|
| Rust 工作区（9 crate，依赖单向） | `cargo test --workspace` 全绿（139 用例） |
| `.pbt` 格式 v1 + Rust/Python 双实现 | 两侧黄金字节用例通过；`test_python_reads_a_trace_written_by_rust` 通过 |
| 重复折叠压缩（可逆） | `fold_then_expand_is_lossless`（两侧）+ demo 里 4069 → 71 记录无损还原 |
| 控制权/能力/epoch 模型 | `stale_epoch_commands_are_rejected_after_takeover` 等 8 个权限用例 |
| MCP stdio 服务 + 工具表 | `serve_stdio_reads_until_shutdown`；`tools/list` 按能力过滤 |
| 端到端自检 | `pinbridge demo` 在 Windows 与 Linux 的 CI 上都通过 |
| C ABI 布局契约 | `abi_selfcheck.cpp` 的 `static_assert` + Rust 侧布局单测 |
| 文档体系（架构/设计/规范/流程/ADR） | 25+ 篇；`docs/README.md` 索引可达 |
| CI 门禁 | fmt / clippy / test / e2e / python / changelog / commit-lint 全绿 |

**M0 的桩**（全部显式返回 `not_supported`）：

| 能力 | 位置 |
|------|------|
| 真实内存读写 | `pb_memory_read` / `pb_memory_write` |
| 断点 / 单步 / 继续 / 暂停 | `pb_set_breakpoint` / `pb_single_step` / `pb_continue` / `pb_pause` |
| 脚本执行 | `pb_load_script`（能力校验已生效，CPython 宿主未接） |
| Pin tool 本体 | `native/src/pinbridge_tool.cpp`（不存在，M1 添加） |

## M0.5 — C++ 桥接层落地（✅ 已完成，2026-10-16）

**目标**：在没有 Intel Pin 的情况下，让"Hub ↔ agent"这条链路**真实跑起来**，
从而把 M1 里"第一次真正执行权限链路"的风险提前消掉。

起因是一次工具链探测：本机其实装了 Visual Studio（`cl.exe` / Ninja / vcvars64），
于是 C++ 层不再是"写出来等以后编译"的代码，而是可以当场验证的产物。

| 交付 | 验收（全部可执行） |
|------|--------------------|
| `native/` 能被真实构建 | `pwsh scripts/build-native.ps1`（首次就抓出 3 个真缺陷：缺 `<stddef.h>`、MSVC 未加 `/utf-8` 导致中文字符串被 GBK 误解、ctest 目标路径解析错误） |
| C ABI 布局断言进编译期 | `abi_selfcheck.cpp` 的 `static_assert` 在 MSVC 上编译通过并由 CTest 执行 |
| `pinbridge_bridge.dll`：导出 Hub 期望的 8 个 tool 符号并转发给真实 agent | `ctest` 的 `bridge_roundtrip` 通过（含 3 条事件跨两个语言边界后逐字段完好） |
| 反向用例：agent 缺失时必须明确失败 | `ctest` 的 `bridge_missing_agent` 通过（错误文本给出**尝试过的路径**） |
| 能力闸门跨语言生效 | 只读基线（257）调用 `pause` → `CapabilityDenied{needed: ProcessControl, held: 257}`；给出全量能力位后 → `NotSupported`（两道闸门可区分） |
| **Hub 侧加载器端到端** | `crates/pinbridge-sys/tests/bridge_end_to_end.rs`：真实 `LoadLibraryA` → 校验 ABI v1 → `attach` → 下发能力 → 驱动事件 → 取回错误文本 |
| **Hub ↔ agent 回环链路** | `crates/pinbridge-agent/src/link.rs`（服务端）+ `pinbridge-proto::agent_link`（两端共用的协议与能力表）+ `crates/pinbridge-client/tests/agent_link_end_to_end.rs`：真 socket 握手、版本协商、**双端闸门可区分**、事件过链路、对端消失不阻塞 |
| 跨平台 lint 不再靠 CI 抓 | `scripts/verify.ps1` 内置 `cargo clippy --target x86_64-unknown-linux-gnu` |

**这一阶段顺带修掉的真实缺陷**（都是"跑起来才暴露"的）：

1. `pinbridge-sys` 在能力不足时把 `needed` 固定填成 `MemRead`——AI 会去申请一个
   它其实已经有的能力。现在 `to_error`/`check` 要求调用方传入真实需要的能力，
   `Api` 也记住"下发过的能力集"以便如实回报 `held`。
2. `Status::NotSupported` 之前映射成 `Error::Trace`——读日志的人会去翻轨迹文件，
   而正确方向是"查哪一层没实现"。现在映射为 `TargetUnavailable` 并说明原因。
3. C++ 侧字符串常量被 MSVC 按 GBK 解释，报出"字符串字面量中的换行符"这种
   与真正原因无关的错误。修法是 `/utf-8`，并写进了规范。

## M1 — 接通 Pin 与脚本运行时（进行中）

**目标**：让 M0 的桩变成真能力，且**权限链路第一次真正执行**。

**Pin 已就位并跑通**（2026-10-16）：`third_party/pin/` 是实测过的 3.31 kit
（`pin-3.31-98869-fa6f126a8`，zip 的 sha256 记在
[third_party/pin/README.md](../third_party/pin/README.md)）。本机是 AMD Ryzen 7
9700X——"AMD 上不受官方支持"这件事已实测澄清，结论与边界写在
[ADR 0010](adr/0010-pin-now-dynamorio-as-swappable-probe-layer.md)。

| 任务 | 状态 |
|------|------|
| ~~`pinbridge-sys` 加载器端到端~~ | ✅ M0.5 已完成（真实 DLL，真实 ABI 校验） |
| ~~agent 加载与调用转发的桥接拓扑~~ | ✅ M0.5 已完成（`agent_bridge.cpp`，Pin tool 复用同一拓扑） |
| ~~Hub ↔ agent 回环链路（真实握手、版本协商、重连）~~ | ✅ M0.5 已完成（`link.rs` + 两端共用协议；连接断开后重新 `ping` 即可，状态属于进程而不属于连接） |
| ~~`pinbridge.dll`：Pin tool 骨架 + **入口点探针**~~ | ✅ 已完成：`pin -t pinbridge.dll -- pb_sample_target.exe` → 命中入口 → 事件穿过桥接层与 agent → **目标进程内写出 `.pbt`** → 我们的读取器解析通过（`ctest -R pin_tool_instruments_a_target`） |
| ~~脚本策略与生命周期（登记 / 白名单 / 状态机 / 执行预算记账）~~ | ✅ 已完成：`crates/pinbridge-agent/src/script.rs`（静态扫描带行号拒绝，**明确说明它不是沙箱**；登记 ≠ 运行由 `receives_events` 与 `is_live` 两个判断分开表达） |
| ~~系统调用 Hook（syscall_enter）~~ | ✅ 已完成：探针识别 `syscall`（实测 59–61 个插装点），采集 SSN + 前四个参数（x64 第一个参数在 **R10**）。**回调只做内存入队，跨边界由 Pin 内部线程每 10 ms 冲刷**——这个约束是实测逼出来的（回调里调外部 DLL → 5/5 崩溃；改成入队 → 6/6 通过）。`ctest -R pin_tool_captures_syscalls` 断言三件事：捕够 300+ 条、溢出丢弃为 0、Pin 无内部故障 |
| ~~syscall_exit 事件~~ | ✅ 已完成：`sysret` 是内核态指令（插装不到），`INS_Next(syscall)` 也无效（syscall 是基本块终止指令）——返回地址改为**算出**（`syscall 地址 + INS_Size`）并在插桩期按地址识别。实测 441 enter / 454 exit；**16 次返回没有配对的进入**（`int 2Eh` 回退路径共用同一个 `ret`）记为 `args[0]=0` 并计数。ctest 要求进入与返回都 ≥300，且 `exit=0` 直接判失败 |
| ~~内存访问观测（mem_read / mem_write）~~ | ✅ 已完成（**默认关闭**）：三层过滤（模块 / 地址范围 / 读或写），写入事件用"BEFORE 记地址 + AFTER 读值"两段式拿到**写入后**的值。实测靶子执行 2000 次写入、探针捕获 2000 条（一个不多一个不少），零丢弃；`ctest -R pin_tool_mem_range_filter`（脚本化：地址随 ASLR 变化，必须先问靶子再跑探针） |
| ~~指令模板匹配（pattern_hit）~~ | ✅ 已完成：`-pb_pattern "id:name:hex"`，只扫主模块可执行段，命中点按**指令地址范围**匹配，`?` 通配、每模板上限可调（截断必报）。实测靶子带 8 字节魔数、调 500 次 → **命中 500 次**；`ctest -R pin_tool_pattern_hit` |
| ~~TF 单步虚拟化（`flags_virtualize`）~~ | ✅ 已完成：`-pb_tf off\|watch\|virtualize`。`popfq`/`iretq` 在**栈上副本**里剥掉真实 TF，`pushfq` 在 AFTER 点把虚拟 TF 补回读副本。实测四档签名可证伪：单独跑靶子 `real_steps=50`；Pin 下不虚拟化则进程以 `0x80000004` 终止；virtualize 下 **`real_steps=0 tf_reads=50`**（目标确信自己在单步，其实没有）。`ctest -R tf` 三档覆盖 |

| ~~agent 内嵌 CPython（解释器宿主）~~ | ✅ 已完成：**运行时动态加载**（`LoadLibraryA` + `GetProcAddress`，`python3.dll` → `python314.dll`，可用 `PINBRIDGE_PYTHON_DLL` 指定）。链接导入库会让 agent 在没装 Python 的机器上**整个加载不了**，所以不链接。实测在进程内跑 `assert 6*7==42`、异常路径明确报错、指定不存在的库时不静默回退 |
| ~~`pb` 模块（subscribe / log / mark）~~ | ✅ 纯 Python 注入（不写 C 模块：结构体布局写错就是目标里的随机崩溃）。队列编码 `[kind, value, text]`；两个窄入口 `_pb_drain_json` / `_pb_dispatch` |
| ~~探针回调分发（事件 → 脚本函数）~~ | ✅ `pb_push_event` **先分发再入队**（让脚本输出紧跟触发它的事件）；事件名与 `EventKind::name()` 同一张表；订阅了未知事件名会报错而不是静默 |
| ~~`pb.read`~~ | ✅ 已完成（反向调用） |
| ~~脚本换版本/卸载时收回处理器~~ | ✅ 订阅与脚本标记绑定，`UnloadScript` 从桩变实现——否则旧处理器仍在被调用，表现为"我改了脚本行为却还是旧的" |
| ~~`pb.snapshot`~~ | ✅ 分块读+分块写+校验和，256 MiB 上限，失败指出"从哪个偏移起读不到"，写审计行 |
| ~~`pb.defer`~~ | ✅ 回调里只排队，宿主在回调返回后执行；单个出错不带走整批，异常进审计通道 |
| ~~脚本执行时间预算的**强制**中断（超时 → `TimedOut`）~~ | ✅ 看门狗线程 + `PyThreadState_SetAsyncExc` 异步注入（**不用 `sys.settrace`**：实测"超时中断过一次之后再次 settrace 收不到行事件"，看门狗会静默失效）。超时 → `Status::Timeout` + 不注册 + 标记 `TimedOut` + 审计行。已知边界：阻塞式 C 调用打断不了 |
| ~~真实内存读写~~ | ✅ 已完成：`ReadProcessMemory` / `WriteProcessMemory`（**不解引用**：不可读地址要能"失败"，而不是把目标打崩；也**不临时改保护属性**——那会改变目标状态）。部分写入按失败处理；错误文本带地址 + Windows 错误码。实测：写 11 字节读回一致、不可读地址报错不崩、被拒绝的写不落盘 |
| ~~脚本侧 `pb.read`~~ | ✅ 已完成：反向调用（`PyCFunction_NewEx` + 静态 `PyMethodDef`），能力闸门在 Rust 侧，拒绝抛 `RuntimeError` 而不是返回空数据。实测脚本读到真实内存并用 `mark` 报回魔数 |
| ~~断点~~ | ✅ 已完成：软件断点（`0xCC` + 向量化异常处理器）。**命中一次后自动解除**（那段"恢复→单步→重装"的舞蹈在多线程目标里有真实竞态，而我们没有停下目标线程的能力——见 [design/breakpoints.md](design/breakpoints.md)）。地址打错会**报错而不是打崩目标**。实测：命中一次并记录线程号、函数仍正常返回、原字节已写回 |
| ~~继续 / 暂停~~ | ✅ 已完成，语义是**采集**的暂停与继续（不是冻结目标）：停写轨迹/入队/喂脚本，**跳过条数可查**，状态变化事件自己不被跳过；重复调用明确说"没有影响"；附加时重置会话状态 |
| 单步 | ⏳ **明确不做调试器语义**（需要"停下目标所有线程"，Pin 给不了）。桥接层已导出 `pb_single_step`，于是调用方拿到的是诚实的"未实现"而不是"符号缺失"；替代做法见 [design/script-runtime.md](design/script-runtime.md)。指令级逐步记录属于 M2 |
| ~~Hub 侧加载器解析 `pb_start_trace`/`pb_stop_trace`~~ | ✅ 已完成：`pinbridge-sys` 增加 `start_trace`/`stop_trace`（符号解析 + 参数校验 + 能力/状态码翻译），端到端测试**真的加载 DLL 并录出一份 `PBT1` 魔数的 `.pbt`** |
| ~~Hub 会话缺少到 agent 的 client 句柄~~ | ✅ 已完成：Hub 定义 `TargetLink` trait（测试可注入假实现），会话持有可选链路，新增 `attach_link` / `read_memory`（参数校验 → 能力校验 → 会话状态 → 链路，顺序刻意如此）；组合根（MCP）提供 `ClientLink<T>` 把 `pinbridge_client::AgentClient` 适配成链路；`pb_memory_read` 接通。测试覆盖：能力闸门、没挂链路时的错误必须点明缺链路、长度不符必须报错（不许截短）、链路层 hex 解码与错误翻译 |
| ~~MCP `pb_modules`~~ | ✅ 已完成：agent 侧走 **PEB 加载器链**（`gs:[0x60]` → `Ldr+0x10`）进程内枚举，不依赖 psapi；快照读法不持 loader lock，因此**跳过的不合法条目数会如实上报**。链路新增 `Modules` 请求、Hub 新增 `list_modules`、`pb_modules` 工具接通。实测枚举到 9 个模块（完整路径、主模块在首位） |
| ~~寄存器捕获段~~ | ✅ 已接通：入口探针用 `IARG_REG_VALUE` 取 7 个寄存器 + `IARG_INST_PTR` 取 RIP，随事件交给 agent；端到端断言 `registers=1 rip=0x…`（RIP 必须落在靶子主模块内） |
| ~~MCP `pb_registers` 工具~~ | ✅ 已接线（proto 工具描述 → hub 闸门 → MCP link → 工具处理器）；用例覆盖"有快照给值 / 没快照说清哪些线程有 / 线程号越界拒绝" |
| ~~MCP `pb_modules` 工具~~ | ✅ 早已接线（本轮核对过，roadmap 这一行是过期的） |：agent 活在目标进程里，拿不到别的线程上下文） | |
| ~~MCP 工具 `pb_modules` / `pb_registers`~~ | ✅ 均已接线 |

### 明确不做（占位）

**VMP / 强壳的自动化分析不做**，先占位。边界与理由：

* 它要解决的是另一个问题（定位虚拟机 dispatcher、还原 handler 语义），
  与"四类插桩点 + 事件模型"不是同一条路径；先塞进来会把 M1 拖成没有边界的黑洞。
* 现有能力对它**部分是通用的**（轨迹录制、内存读写、模板匹配可以做锚点），
  所以定位是"暂不承诺"，而不是"架构上做不到"。
* 真要做就单独立项，并且**先用自写样本**复现出一个虚拟机，再谈自动化——不去分析
  来路不明的壳样本。

**测试靶子一律自写**：`native/tests/sample_target.cpp` 就是模板。不拿系统二进制
或外部样本当靶子——那样"测试失败"的原因会同时包含"我们的工具坏了"与
"那个二进制有反调试"，两者必须能分开。

| 验收标准 | 判据 |
|----------|------|
| ~~`pin -t pinbridge.dll -- TARGET.exe` 能跑起来并产出事件~~ | ✅ 自动化：`ctest -R pin_tool_*`（入口探针 / 系统调用 / 内存访问 / 模板命中 / TF 三档） |
| 一段脚本能被加载、被回调、产生 `script_output` 事件 | 用例 |
| 尝试 `import os` 被拒且留下审计 | 用例 |
| ~~死循环脚本在预算内被终止，目标仍可继续~~ | ✅ 用例（`a_dead_loop_script_is_stopped_inside_its_budget_and_the_target_keeps_working`） |
| ~~尝试 `import os` 被拒且留下审计~~ | ✅ 用例（拒绝与超时都写 `.pbt` 的 AUDIT 块） |
| ~~被插装进程内驱动 agent~~（脚本被事件驱动、断点命中、内存读写） | ✅ 自动化：`ctest -R pin_tool_agent_end_to_end`，断言摘要行 `PB_AGENT_CHECK script_output=1 ssn_mark=1 breakpoint_hit=1 memory=1` |
| ~~继续 / 暂停~~ | ✅ 已完成（采集语义，见设计文档） |
| 单步 | ⏳ 明确不做调试器语义；桥接层已导出，调用方拿到诚实的"未实现" |

**M1 的关键风险**：

1. **权限链路首次真正执行**——因此桩函数已经强制"先校验能力再报 not_supported"，
   并有测试覆盖这个顺序；
2. 脚本跑在目标线程上会影响目标时序——回调必须极快返回，重活交给 `defer`；
3. Pin 的版本差异（3.31 内部的 API 变动）——`pinbridge-sys` 的加载器会
   拒绝 ABI 不匹配的 DLL，不做"尽力兼容"。

## M2 — 四类介入时机全部落地（规划中）

| 任务 | 验收 |
|------|------|
| 系统调用 Hook（进入/离开） | 脱壳场景能观察到"改内存属性"与"写盘" |
| Windows 异常时机接管 | 目标用异常做控制流时事件流连续，且不依赖调试框架 |
| 指令模板匹配 Hook | 模板命中产生 `pattern_hit`，且模板 ID 可追溯 |
| **TF 虚拟化** | 目标设 TF 时出现 `trap_flag` 事件，异常位置与原生执行一致（手工验证） |
| 每线程虚拟 TF 状态机 | 用例：两个线程各自的 TF 互不干扰 |
| 轨迹实时流式回传（边录边给 Hub 读） | 大轨迹不再需要等 `finish` |
| 事件前置过滤（在产生之前筛掉噪音） | 同一场景下 `dropped` 从非零降到 0 |

## M3 — 离线深度分析与工作台（规划中）

| 任务 | 验收 |
|------|------|
| 反向切片（从某点回溯影响它的写入） | 给定 OEP 候选，能列出影响它的写入链 |
| 字节级污点传播（携带偏移与长度） | 与结构化近似版本对比，误报显著减少 |
| 调用图重建 | 从 `reg_write` + 栈事件还原主要调用关系 |
| 多轨迹差分（同一目标两次运行） | 能指出"哪次行为不同""差异在哪个序号之后" |
| Tauri 工作台接真实数据 | 事件流、溯源、脚本版本、接管按钮全部可用 |
| 轨迹可视化（时间线 + 地址热力） | 5 分钟量级轨迹能在 < 2s 内渲染出概览 |

## 不做的事（长期）

| 不做 | 理由 |
|------|------|
| 远程控制面 | agent 链路只允许回环（见 [architecture/security-model.md](architecture/security-model.md)） |
| 自更新 / 代码下传 | 调试工具的自助升级等于给它一个永不失效的入口 |
| 稀疏采样式压缩（只留代表事件） | 会丢证据，与本项目"轨迹是唯一真相"的立场冲突 |
| 内置样本 | `.gitignore` 里 `samples/` 是硬禁止 |
| macOS 支持 | Pin 不提供 |

## 优先级原则

同一时间只推进一个里程碑。若中途插入紧急修复：

1. 修 bug 优先于新功能（尤其权限与格式相关的）；
2. 修完必须回到原里程碑，**不允许**把 M1 的活儿摊到 M2 里"顺手做"；
3. 里程碑范围变化要更新本文件并在 PR 里说明——路线图过期比没有路线图更糟。
