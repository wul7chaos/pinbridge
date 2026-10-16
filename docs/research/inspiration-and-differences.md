# 灵感来源与本项目自己的判断

PinBridge 的起点是看雪论坛的一篇文章。本文件逐条说明
**哪些是原文的意图、哪些是我们自己定的**，以及我们**刻意改掉**的地方。

* 原文：[《AI 时代逆向分析：动态分析框架》](https://bbs.kanxue.com/thread-293115.htm)
  （作者 BitWarden，2026-10-01）
* 本地存档：[kanxue-293115-ai-dynamic-analysis-framework.md](kanxue-293115-ai-dynamic-analysis-framework.md)
  （含原始 HTML：[research/kanxue-293115-raw.html](research/kanxue-293115-raw.html)）
* 图片：[imgs/](imgs/)（原文流程图与功能截图的本地快照）

> 原文的仓库链接已被作者编辑移除，页面中不再存在。
> 因此本项目是**基于文章公开的设计思路独立实现的**，
> 不包含、也不声称包含原作者的任何代码。

## 一、直接采纳的

| 原文意图 | 我们的落点 |
|----------|-----------|
| 基座用插装框架（Pin / DynamoRIO），不用 Windows 调试框架 | [ADR-0001](adr/0001-instrumentation-over-debug-api.md) |
| AI 主导、人类辅助；人与 AI **平级** | `Principal`（`human:*` / `ai:*` 同构） |
| 以代码驱动：通过 MCP 把代码动态注入框架执行 | `pb_script_load` + 内嵌 CPython（M1） |
| 四类介入时机（断点 / Windows 异常 / syscall Hook / 指令模板） | [design/injection-points.md](design/injection-points.md) |
| 有界事件通道 | [ADR-0009](adr/0009-bounded-channel-drop-accounting.md) |
| 执行轨迹录制 + 离线重放 | [specs/pbt-format.md](specs/pbt-format.md) + [design/offline-replay.md](design/offline-replay.md) |
| trace 压缩：重复段折叠（`same_payload` 不比序号） | [design/trace-compression.md](design/trace-compression.md) |
| 寄存器未变化就不记录 | 记录的 `argc` 省略末尾未变化参数 |
| 用 Python 而不是 JS 解释器 | [ADR-0002](adr/0002-script-first-over-tool-menu.md) |
| 主要用 Rust 构建，Pin 层用 C/C++ 兼容层 | [ADR-0004](adr/0004-rust-core-narrow-c-abi.md) |
| TF 陷阱的处理（虚拟化，而不是清掉或放任） | [design/flags-virtualization.md](design/flags-virtualization.md) |
| 理想状态：动静配合、无人值守，人只在外观察 | 人类默认是观察员；`takeover` 是唯一的人工阀门 |

## 二、我们补上的

原文把重点放在"思路"，明确不展开实现（作者认为 AI 时代把仓库交给 AI 扫一遍即可）。
这些是我们自己定的：

| 我们补的东西 | 为什么必须有 |
|--------------|--------------|
| **`.pbt` 二进制格式的完整规范** | 原文只有"做了几种压缩算法与策略"。没有规范就无法验证、无法跨实现 |
| **Rust 与 Python 双实现 + 黄金字节** | "轨迹是唯一真相来源"要求它能被独立验证 |
| **控制权 epoch 模型** | 原文提到"控制权与权限"，但没定义"AI 的在途命令如何失效" |
| **能力位与授权矩阵** | 让"AI 默认最小权限"成为可测试的性质，而不是口号 |
| **双端校验（Hub + agent）** | 单端校验在链路存在的情况下不构成边界 |
| **丢数据必须计数上报** | 否则 AI 会把"我看到的"当成"发生过的" |
| **失败也要留痕的溯源** | AI 的失败尝试是理解它策略变化的唯一线索 |
| **桩函数"先校验能力再报不支持"的顺序** | 否则权限链路在接通真实实现前从未被执行过 |
| **C ABI 的编译期断言** | 跨语言结构体布局漂移是最难排查的一类崩溃 |
| **完整开发规范（分支/提交/评审/ADR/测试）** | 原文是个人项目；我们要让它能被协作维护 |
| **CI 门禁与端到端自检进 CI** | "骨架断了但单测全绿"是常见状态 |

## 三、我们刻意改掉的

| 原文做法（或倾向） | 我们改成 | 理由 |
|--------------------|----------|------|
| 仓库地址给出去、让人自己看代码 | 全量文档 + ADR + 规范 | 我们相信"为什么这样做"必须被写下来，否则三个月后没人敢改 |
| 描述上以 Windows 平台视角为主 | 平台相关部分（Pin、TF）明确标注 Windows；其余（模型/格式/协议）平台无关 | Linux 侧的 Rust 部分同样可构建可测试 |
| 四类时机对 AI 可能"冗余"（原文承认） | **保留四类**，但在事件类型层面统一 | 人类复核时需要知道"这是哪一层的观察结果"；AI 只关心数据在不在 |
| 未展开的错误处理/权限细节 | 显式能力集 + epoch + 双端校验 + 溯源 | 执行者是 AI，试错频率远高于人 |
| trace 压缩只讲了策略 | 定义**可逆**折叠（含严格校验模式） | 不可逆的压缩产物不能作为分析证据 |
| 未提及测试与验证方式 | 三层测试 + 跨语言互读 + 端到端 demo 进 CI | 没有验证的框架在对抗场景里等于没有框架 |

## 四、我们保持开放的分歧

以下几处我们**不确定自己的选择更好**，写在这里供后续讨论：

1. **Pin vs DynamoRIO**：我们以 Pin 为准（文档与生态更成熟），
   但 DynamoRIO 开源、可审计、许可更宽松。如果 Pin 的许可成为障碍，
   切换成本主要在 `native/`（C ABI 与 agent 不变）。
2. **内嵌 CPython 的体积与可检测性**：把解释器带进目标是明确的代价
   （内存 + 检测面）。替代方案（WASM 沙箱、RPC 出去执行）我们都评估过，
   但都牺牲了"跑在目标线程上下文里"这个关键能力。
3. **折叠标记的语义**：我们把它形式化成"标记落在段尾、`args[0]` 是被折掉的条数"，
   并保持可逆。原文片段在上下文不全的情况下可能还有别的等价表述——
   如果有更自然的定义，欢迎在 Discussion 里指出。
4. **不设覆盖率门禁**：我们认为在这个领域里"覆盖率高的垃圾测试"危害更大，
   但这依赖评审的质量。如果评审跟不上，可能需要补一些机械性检查。

## 五、出处与致谢

* 设计灵感来自 BitWarden 的公开文章（链接见文首）；
* 本项目独立实现，代码与文档均为原创；
* 引用原文的片段（例如折叠判定与 TF 处理的代码样式）在
  [design/trace-compression.md](design/trace-compression.md) 与
  [design/flags-virtualization.md](design/flags-virtualization.md) 中已明确标注来源与出处；
* 如果你认为本项目的某处表述与原文意图不符，请开 issue——
  我们宁可修正文档，也不愿意留下一个"看起来像原创但其实误读了别人"的设计。
