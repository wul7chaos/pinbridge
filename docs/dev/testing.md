# 测试策略
## 已自动化：被插装进程内的 agent 端到端

```
pwsh scripts/build-native.ps1 -WithPin -RequireAgent
ctest --test-dir native/build-pin -R pin_tool_agent_end_to_end -V
```

期望看到：

```
PB_AGENT_CHECK script_output=1 ssn_mark=1 breakpoint_hit=1 memory=1 target_value=22
```

这条用例在**被 Pin 插装着的进程**里做四件事（`native/tests/sample_target.cpp`
的 `--agent-check` 模式）：加载 agent DLL → 加载 `native/tests/agent_check_script.py`
→ 推一条系统调用事件（脚本应当把 SSN 报回来）→ 下断点并触发（函数命中后仍要
正常返回 22）→ 真实内存写入后读回核对。

没装 Python 时打印 `PB_AGENT_CHECK SKIP no-python` 并以 77 退出
（ctest 的 `SKIP_RETURN_CODE`）：**跳过要说清原因**，不能和"实现坏了"混成一个失败。

### 仍未自动化（手工清单）

* 单步 / 继续 / 暂停——语义尚未落地，见 [design/script-runtime.md](../design/script-runtime.md)；
* `pb_registers`——真实实现属于 Pin 工具侧（agent 拿不到别的线程上下文）。


## 我们的测试在防什么

PinBridge 的失败不是"页面显示错了"，而是：

1. **目标进程崩了**（分析工具把分析对象搞死了）；
2. **权限漏了**（AI 拿到了它不该有的能力）；
3. **证据错了**（轨迹还原出的时间线与现实不符，而结论建立其上）；
4. **两侧分叉了**（Rust 与 Python 对同一份文件给出不同结果）。

所有测试都是围绕这四件事组织的。覆盖率不是目标——
"有没有测到失败方向"才是。

## 分层

| 层 | 位置 | 数量级 | 跑多久 | 依赖 |
|----|------|--------|--------|------|
| 单元 | `crates/*/src/**` 的 `#[cfg(test)]` | 130+ | < 5 s | 无 |
| 跨模块 | 同上（hub 的端到端用例） | 少量 | < 5 s | 临时目录 |
| 端到端自检 | `pinbridge demo`（CI 里跑） | 1 | < 5 s | 无 |
| 跨语言 | `python/tests/test_rust_parity.py` | 少量 | < 30 s | 已构建的 CLI |
| ABI 布局 | `native/tests/` + `static_assert` | 少量 | 编译期 | 无（不需要 Pin） |

**全部离线可跑**。没有 Pin、没有样本、没有网络。

## 必测的失败方向（这是评审清单）

新增功能时，请对照这张表检查：

| 方向 | 例子（已有用例） |
|------|------------------|
| 权限不足被拒 | `ai_starts_read_only_and_cannot_escalate_itself` |
| 越权尝试**也**被拒 | `ai_cannot_take_over_control` |
| 旧 epoch 失效 | `stale_epoch_commands_are_rejected_after_takeover` |
| 上限约束生效 | `grants_are_bounded_by_the_session_ceiling` |
| 非法状态迁移被拒 | `state_machine_rejects_illegal_transitions` |
| 背压丢弃被计数 | `channel_is_bounded_and_counts_drops` |
| 未实现显式报错 | `abi_surface_smoke`（能力 OK → `not_supported`） |
| 空指针 / 越界参数 | `abi_surface_smoke` 的 §6 |
| 外部输入损坏 | `bad_magic_and_version_are_rejected`、`oversized_varint_is_rejected` |
| 截断可定位 | `truncated_record_is_reported_not_swallowed` |
| 自相矛盾被拒 | `checked_expansion_rejects_inconsistent_marker` |
| 跨语言一致 | `golden_event_payload_hex_matches_python`（两侧） |
| 真实互通 | `test_python_reads_a_trace_written_by_rust` |

**只测 happy path 的 PR 会被要求补用例。**

## 几条测试纪律

### 1. 断言必须带消息

```rust
assert_eq!(folded.len(), 2, "1000 条相同连续事件应折叠成 2 条");
```

失败时读者要能立刻知道**期望的是什么语义**，而不是去翻规范。

### 2. 不要为了通过而改断言

修 bug 的正确顺序是：**先加一个能复现的失败用例 → 再修**。
这是唯一能证明"真的修好了"的方式，也是回归防护。

### 3. 测试失败时先怀疑实现

本仓库的测试都是从规范推导出来的。如果你发现"测试写错了"，
请先确认规范是怎么说的；如果确实要改断言，在 PR 里说明为什么。

### 4. 跨语言用例必须真的跑

`test_python_reads_a_trace_written_by_rust` 在找不到构建产物时会 skip。
**CI 里必须先 `cargo build`**（见 `.github/workflows/ci.yml` 的 `python` 任务），
否则这组"最值钱"的用例会被静默跳过。

### 5. 全局状态的测试要串行

`pinbridge-agent` 的状态是模块级单例（agent 只有一个实例）。
涉及它的测试用 `GLOBAL_STATE_LOCK` 互斥，并且**自己显式设置初始状态**
（不依赖某个测试先跑过）。

## 夹具与样本

* **不提交样本**。`.gitignore` 里 `samples/` 是硬禁止；
* 单元测试的夹具是**合成数据**（例如"4000 条相同 syscall + 64 条内存写入"）；
* 需要真实轨迹时，用 `pinbridge demo` 现场生成（CI 就是这么做的）。

合成夹具的好处：可复现、无法律风险、能精确构造边界情况。

## 覆盖率

我们**不设覆盖率门禁**。理由：本项目的价值集中在"少数关键路径的正确性"
（权限、编解码、可逆性），覆盖率高的垃圾测试只会给人虚假的安全感。

作为参考，`cargo test --workspace` 当前是 139 个用例。

## 怎么加一个新测试

1. 想清楚它属于哪一层（单元 / 跨模块 / 端到端 / 跨语言）；
2. 名字写清**行为 + 条件**；
3. 至少有一个断言针对**失败方向**（如果这个功能有失败方向）；
4. 夹具尽量合成；
5. 跑 `pwsh scripts/verify.ps1` 确认没有破坏别的层。

## 手工验证（无法自动化的部分）

M1 接通 Pin 之后，以下必须在**真机**上手工验证，并把结果贴到 PR：

| 场景 | 期望 |
|------|------|
| 附加到一个普通 x64 程序 | 事件流正常、目标不卡顿 |
| 目标设置 TF | 出现 `trap_flag` 事件；异常位置与原生执行一致 |
| 目标进入长循环 | 轨迹可折叠；`dropped` 为 0 或已如实上报 |
| 目标崩溃 | 轨迹文件缺少 SUMMARY 段；读侧报"不完整"而不是"损坏" |
| 人类 takeover | AI 的下一次写操作收到 `control_conflict` 且带 hint |

这些场景需要目标程序与 Pin，无法进入 CI；因此它们是**发布前的手工检查项**
（见 [build-and-release.md](build-and-release.md)）。
