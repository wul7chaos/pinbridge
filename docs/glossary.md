# 术语表

本项目的几个词是自造的，含义必须精确。按"你可能在哪里遇到它"排列。

## 主体与权限

| 术语 | 定义 | 代码位置 |
|------|------|----------|
| **主体（Principal）** | 谁在操作：`human:*` / `ai:*` / `system`。人与 AI **平级**。 | `pinbridge-core/src/principal.rs` |
| **能力（Capability）** | 一个具体动作的许可位，共 12 项（`mem_read`…`capability_grant`）。 | `pinbridge-core/src/capability.rs` |
| **能力集（CapabilitySet）** | 位图。默认最小：AI 一进来只有 `mem_read` + `trace_record`。 | 同上 |
| **会话上限（ceiling）** | 会话级的授权上限。授权 = `requested ∩ ceiling`。 | `Session::ceiling` |
| **控制权租约（ControlLease）** | "此刻谁说了算"。带 `epoch` 与过期时间。 | 同上 |
| **epoch** | 控制权代数。单写者移交时 +1；**旧 epoch 的命令会被拒绝**。 | 同上 |
| **接管（takeover）** | 人类收回控制权并推进 epoch。**只有人类能做**。 | `HubService::takeover` |
| **双端校验** | 能力在 Hub 与 agent 两处各校验一次。 | hub + agent |

## 会话与运行时

| 术语 | 定义 |
|------|------|
| **会话（Session）** | 一个目标进程 + 一组脚本 + 一份轨迹 + 一份权限与审计记录。**一个会话一个目标**。 |
| **状态机** | `Created → Attached → Running ⇄ Paused → Detached → Closed`（含 `Failed`）。 |
| **观察窗口** | Hub 保留的最近 N 条事件（默认 1 万）。**会滚**，滚掉的条数会计数。 |
| **事件通道** | agent → 脚本/上层的实时流。有界，满了按策略丢弃并计数。 |
| **脚本（Script）** | 注入目标进程的 Python 代码。状态：`Loaded` / `Superseded` / `Unloaded` / `TimedOut`。 |
| **Superseded** | 同名脚本被新版本取代。**不立即卸载**（可能还有在途回调）。 |
| **脚本时间预算** | 单次回调的时间上限（默认 5 s）。超时 → `TimedOut` + 强制卸载。 |
| **agent** | 注入目标进程的那一半（Rust cdylib）。 |
| **Pin tool** | C++ 那一半（`pinbridge.dll`），负责探针与上下文控制。 |

## 事件与轨迹

| 术语 | 定义 | 代码位置 |
|------|------|----------|
| **事件（Event）** | 唯一的"事实单位"。`{sequence, kind, thread_id, address, args[0..8]}`。 | `pinbridge-trace/src/event.rs` |
| **序号（sequence）** | 全局单调递增，从 1 开始（0 是保留值）。**顺序的唯一依据**（不是时间戳）。 | 同上 |
| **载荷（payload）** | 事件的类型 + 线程 + 地址 + args。**不含序号**——序号正是被折叠掉的信息。 | `Event::same_payload` |
| **折叠（fold）** | 把"序号连续 + 载荷相同"的连续事件压成"代表 + 标记"。 | `pinbridge-trace/src/fold.rs` |
| **折叠标记（REPEAT）** | `{sequence=段内最后一条, args[0]=折叠条数, args[1]=原始类型}`。 | 同上 |
| **展开（expand）** | 折叠的逆操作。**逐字节可逆是硬要求**。 | 同上 |
| **记录（record）** | 文件里的一条（可能是普通事件，也可能是折叠标记）。 | `pbt.rs` |
| **chunk** | `.pbt` 里的段：`tag u8 + length u32 + payload`。同一 tag 可多次出现。 | `pbt.rs` |
| **`.pbt`** | 轨迹文件格式（二进制）。`FORMAT_VERSION = 1`。 | [specs/pbt-format.md](specs/pbt-format.md) |
| **SUMMARY** | 轨迹尾部的汇总段。含 `records` / `events` / `repeat_folded` / `dropped` / `truncated`。 |
| **折叠率** | `repeat_folded / (records + repeat_folded)`。 |
| **dropped** | 被背压丢弃的事件数。**非零即代表轨迹不完整**。 |

## 介入时机

| 术语 | 含义 |
|------|------|
| **断点时机** | 用探针（不是写 `0xCC`）在地址上停下。 |
| **异常时机** | 接管目标自己的异常分发路径。**不依赖 Windows 调试框架**。 |
| **系统调用 Hook** | 记录 syscall 进入/离开。**不知道要找什么时的主力**。 |
| **模板匹配** | 按字节模板匹配指令序列，定位一类行为模式。 |
| **TF 虚拟化** | 把目标的 `TF` 处理在"应用指令边界"上虚拟化，物理 TF 不进代码缓存。 |

## 链路与协议

| 术语 | 含义 |
|------|------|
| **Hub** | 控制面（`HubService`）。不做分析，只做会话/权限/脚本/观察窗口。 |
| **agent 链路** | Hub ↔ 目标进程内的 agent。**只允许回环地址**。 |
| **`req` / `res` 标签** | agent 链路的 JSON 帧标签（`{"req":"read_memory",...}`）。 |
| **WIRE_VERSION** | Hub 侧协议的版本（当前 1）。 |
| **PB_ABI_VERSION** | C ABI 的版本（当前 1）。**不匹配时拒绝加载**。 |
| **MCP** | Model Context Protocol。AI 客户端与本框架之间的标准接口。 |
| **`pb_` 前缀** | 所有 MCP 工具名的前缀，避免与别的 server 撞名。 |

## 我们刻意区分开的说法

| 说法 | 在我们的语境里意味着 |
|------|---------------------|
| "启发式"（heuristic） | 可能给出**线索**，失效条件写在文档里。**不能当证据用** |
| "证据"（evidence） | 能从 `.pbt` 里复现的东西。必须能指回具体的序号 |
| "静默降级" | **被禁止的行为**。未实现必须报错，不是"尽力而为" |
| "策略性沙箱" | 防意外（AI 写出危险代码），**不是安全边界** |
| "桩（stub）" | 先校验能力、再返回 `not_supported`。顺序不能反 |
| "已知限制" | 写在文档里的边界，不是 bug。发现它不成立请开 issue |
| "失效条件" | "什么时候这个结论会不成立"，写在相关文档里 |
