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
| syscall_exit 事件 | ⏳ `INS_IsSysret` 在当前 Windows 目标上没匹配到（`exit=0`），返回路径要另找机制 |
| 内存访问观测（mem_read/mem_write） | ⏳ 需要 `IARG_MEMORYREAD_EA`/`IARG_MEMORYWRITE_EA` + 过滤；**必须复用"入队 + 内部线程"这条路**（高频探针的硬约束） |
| 指令模板匹配（pattern_hit） | ⏳ 同上 |
| TF 虚拟化（`flags_virtualize.cpp`） | ⏳ |
| agent 内嵌 CPython + `pb` 模块（subscribe / log / mark / read / snapshot / defer） | ⏳ 策略层已就绪，缺的是运行时 |
| 脚本执行时间预算的**强制**中断（超时 → `TimedOut`） | ⏳ 记账已完成（`ScriptBudget`），强制需要解释器配合 |
| 真实内存读写 / 断点 / 单步 / 继续 / 暂停 | ⏳ 手工验证清单见 [dev/testing.md](dev/testing.md) |
| Hub 侧加载器解析 `pb_start_trace`/`pb_stop_trace`（让 Hub 能驱动**目标内**录制） | ⏳ 桥接层与 agent 都已就绪，只差 `pinbridge-sys` 的符号表 |
| MCP 新增工具：`pb_modules` / `pb_registers` | ⏳ |

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
| `pin -t pinbridge.dll -- TARGET.exe` 能跑起来并产出事件 | 手工验证 |
| 一段脚本能被加载、被回调、产生 `script_output` 事件 | 用例 |
| 尝试 `import os` 被拒且留下审计 | 用例 |
| 死循环脚本在预算内被终止，目标仍可继续 | 用例 |
| 真实内存读写/断点/单步/继续/暂停 | 手工验证清单 |

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
