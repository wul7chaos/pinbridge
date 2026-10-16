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

## M1 — 接通 Pin 与脚本运行时（规划中）

**目标**：让 M0 的桩变成真能力，且**权限链路第一次真正执行**。

| 任务 | 验收 |
|------|------|
| `pinbridge.dll`：Pin tool 入口、探针注册、agent 加载与调用转发 | `pin -t pinbridge.dll -- TARGET.exe` 能跑起来并产出事件 |
| agent 内嵌 CPython + `pb` 模块（subscribe / log / mark / read / snapshot / defer） | 一段脚本能被加载、被回调、产生 `script_output` 事件 |
| 脚本沙箱白名单（禁 `os`/`ctypes`/`socket`/写文件） | 用例：尝试 `import os` 被拒且留下审计 |
| 脚本执行时间预算（超时 → `TimedOut` + 强制卸载） | 用例：死循环脚本在 5s 内被终止，目标仍可继续 |
| 真实内存读写 / 断点 / 单步 / 继续 / 暂停 | 手工验证清单（见 [dev/testing.md](dev/testing.md)） |
| `pinbridge-sys` 加载器端到端 | `Api::load` 对一个真实构建出的 DLL 成功并校验 ABI |
| Hub ↔ agent 回环链路（真实握手、版本协商、重连） | 断开后能重新 attach，`Detached → Attached` 迁移正确 |
| MCP 新增工具：`pb_modules` / `pb_registers` | 工具表与 `docs/specs/mcp-tools.md` 同步，含"能力不足被拒"测试 |

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
