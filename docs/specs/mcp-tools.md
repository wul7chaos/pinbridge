# MCP 工具规范

唯一真相来源：`crates/pinbridge-proto/src/mcp.rs` 的 `TOOLS` 表。
本文件解释**为什么是这些工具、为什么只有这些**，以及每个工具的行为边界。

## 设计立场：工具层是薄的

传统"调试器 + MCP"把能力做成功能菜单（读内存、读寄存器、下断点……），
AI 只能在这些固定函数的输入输出空间里排列组合。遇到带分支和回退的流程
（脱壳就是典型）就断了。

所以 PinBridge 的工具表只有三类：

| 类别 | 工具 | 作用 |
|------|------|------|
| **开门** | `pb_session_list` / `pb_session_create` / `pb_session_attach` | 建立上下文 |
| **观察** | `pb_events_query` / `pb_memory_read` / `pb_provenance` | 看现状与历史 |
| **投放与收尾** | `pb_script_load` / `pb_trace_record` / `pb_control` / `pb_breakpoint_set` / `pb_capability_grant` | 动手 + 留证据 |

真正的分析逻辑由 AI 写脚本注入目标进程执行（`pb_script_load`）。
脚本能做工具做不到的事：任意循环、条件分支、试错与回退，而且不消耗 token。

## 工具表

| 工具 | 需要能力 | 目标内执行 | 返回 |
|------|----------|-----------|------|
| `pb_session_list` | — | 否 | 会话摘要数组（含控制权持有者与状态） |
| `pb_session_create` | `process_control` | 否 | 新会话 id |
| `pb_session_attach` | `process_control` | 否 | 附加结果（附后默认暂停） |
| `pb_control` | `single_step` / `process_control` | 否 | "已受理" + epoch |
| `pb_memory_read` | `mem_read` | 否 | hex 内存切片（≤64 KiB） |
| `pb_breakpoint_set` | `breakpoint` | 否 | 断点已下 |
| `pb_script_load` | `script_load` | **是** | 脚本 id + 源码 sha256 |
| `pb_events_query` | `mem_read` | 否 | 事件批 + `next` + `dropped` |
| `pb_trace_record` | `trace_record` | 否 | `.pbt` 路径 / summary |
| `pb_provenance` | — | 否 | 审计链（JSON Lines） |
| `pb_capability_grant` | `capability_grant`（**仅人类**） | 否 | 授权后的能力集 |

`executes_in_target = true` 的工具只有一个（`pb_script_load`），
它会在审计里留痕（源码哈希）。这个清单刻意保持最短：
"AI 能在目标进程里执行代码"是能力上限，必须一眼看得见。

## 可见性规则：看不到做不到的事

`tools/list` **只返回当前能力允许的工具**：

```text
只读主体（mem_read + trace_record）看到：
  pb_session_list  pb_events_query  pb_provenance  pb_trace_record  pb_memory_read

人类操作员看到全部 11 个。
```

为什么这样做：让 AI 反复尝试它做不到的事，既费 token 又容易产生
"我是不是该换个办法"的幻觉。**把边界摆在明面上**比让它在边界上撞墙更好。

安全上这不构成依赖：Hub 侧仍然会独立校验能力，
`tools/list` 只是"提前告知"。

## 调用约定

### 一次调用一个目的

工具不支持"顺便多做一件事"。例如 `pb_session_create` 不会顺手 attach——
两个动作合成一个，会让"失败时到底哪一步没做"变得难以回答。

### 写操作返回"已受理"，不是"已完成"

`pb_control` 的 `pause`/`step` 是**异步**的。返回"已受理"之后，
必须用 `pb_events_query` 观察是否落地。这个语义在工具描述里写明了
（"返回的是『已受理』而非『已完成』"），因为 AI 很容易把"受理"当"完成"。

### `dropped` 必须被看见

`pb_events_query` 的响应里永远有 `dropped`：

```text
next=2401 dropped=0
注意：dropped 非零，说明有事件因背压被丢弃；结论必须考虑这一前提。
```

非零时工具会在文本里额外提示一句。这是刻意的"啰嗦"——
一份悄悄少了 3 万条事件的答案，比一个明确的错误危险得多。

## 失败要给出下一步（`hint`）

每个失败都翻译成 AI 能据以行动的文本：

| 失败 | 返回给 AI 的文本（摘要） |
|------|--------------------------|
| 能力不足 | "需要 `script_load`，当前持有 [`mem_read`, `trace_record`]。请让人类通过 `pb_capability_grant` 授权。" |
| 控制权冲突 | "控制权可能在别处被接管（当前持有者 human:operator，当前 epoch 3，你手里的 epoch 是 1）。请重新 attach 获取新 epoch 后重试。" |
| 脚本状态不对 | "运行中的会话不接受新脚本，请先 pause。" |

这就是"可恢复的失败"与"无法理解的失败"的区别。

## 与实现的一致性检查

| 检查 | 位置 |
|------|------|
| 工具名唯一且带 `pb_` 前缀 | `mcp.rs::tests::tool_names_are_unique_and_prefixed` |
| 每个 schema 顶层是 `object` 且能被解析 | `every_tool_has_a_valid_schema` |
| 只有一个工具会在目标内执行 | `only_script_load_executes_in_target` |
| 只有 `pb_capability_grant` 需要授予能力 | `tool_requiring_grant_is_the_only_grant_tool` |
| 工具表与 `tools/list` 数量一致 | `lookup_and_list_agree` |

新增工具时，除了改 `mcp.rs`，还必须：

1. 在 `pinbridge-mcp/src/server.rs` 的 `invoke` 里实现分支；
2. 在 `Capability` 映射里登记所需能力（**不登记就会落到兜底分支，等于失去权限约束**）；
3. 更新本文件；
4. 加一条测试（至少覆盖"能力不足时被拒"）。

## 尚未实现的工具

| 工具 | 状态 | 替代方式 |
|------|------|----------|
| `pb_memory_read` | 骨架（返回 `not_supported` + 指引） | M0 阶段用脚本在目标内读（M1）；现在只能从轨迹里看内存写入的现场 |
| `pb_modules` | 未提供 | 计划在 M1 加：列出模块基址（AI 换算地址需要它） |
| `pb_registers` | 未提供 | 计划在 M1 加 |
| `pb_snapshot` | 未提供 | 计划在 M2 加：按需抓内存快照 |

**未实现就明确报错**，不给一个"看起来能用"的替代品——
后者会让 AI 在错误的前提上继续推理。
