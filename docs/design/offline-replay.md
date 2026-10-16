# 离线重放与深度分析

## 为什么必须有离线这一半

在线插装受两个硬约束：

1. **不能拖慢目标**（拖慢会改变时序，甚至让目标走另一条分支）；
2. **不能在目标进程里分配大内存**（那是被分析对象的地址空间）。

所以在线侧只做"采集 + 折叠 + 落盘"。
一切需要**反复扫描、反复试错、多轮迭代**的推理，都放到 `.pbt` 上做。

额外的好处是**可复现**：同一份轨迹换个算法重跑，结论应该一致；
不一致时至少能定位是"算法变了"还是"样本变了"。

## 工具链

```bash
# Rust 侧：快速体检
pinbridge replay TRACE.pbt --head 20

# Python 侧：完整信息 + 分析
pb-replay TRACE.pbt --head 20
pb-replay TRACE.pbt --taint 0x7FF600003000
pb-replay TRACE.pbt --oep-range 0x140000000-0x140100000
```

```python
from pb.trace.replay import load

trace = load("TRACE.pbt")
print(trace.summary)                      # 完整性判断的第一步
print(trace.expanded_count())             # 展开后的事件数（流式统计）
hits = trace.slice(0x7FF600003000, 0x7FF600004000, limit=50)
candidate = trace.first_write_then_execute(0x140000000, 0x140100000)
```

内存策略：`Trace.records` 是文件里的原始形状（已被折叠），
`trace.events()` 是**流式**展开的迭代器。
一份 5 分钟的 VMP 轨迹展开后可能是几十亿条事件——**不要在没有目的时物化它**。

## 三个分析原语

### 1. 按地址切片（`slice`）

回答"谁动过这块内存"。最常用的开场动作：

```python
for ev in trace.slice(0x7FF600003000, 0x7FF600004000):
    print(ev)
```

### 2. 前向污点传播（`taint_forward`）

回答"这个值从哪来"。

模型：`MemWrite` 的 `args[0]` 是数据来源地址；若来源被标记，
则写入目标也被标记（深度 +1）。

**这是结构化近似，不是字节级追踪**。已知的失效条件：

| 限制 | 后果 |
|------|------|
| 按 4 KiB 页对齐归并 | 同页内不同偏移会被当成同一个地址 |
| 不含算术传播 | `lea rax, [rbx+8]` 这类地址计算看不到 |
| 依赖事件里带数据来源 | 只有部分 Hook 时机能拿到 |
| `mem_read` 默认不订阅 | "读"这一侧天然缺失 |

因此结论要当作**线索**而非**证据**。这也是为什么它出现在文档里而不是
被包装成"taint analysis"这种听起来更权威的名字。

### 3. OEP 候选定位（`first_write_then_execute`）

回答"壳什么时候把代码写完了开始跑"。

判定依据：区间内出现 `MemWrite` 之后，出现 `BreakpointHit` 或 `PatternHit`
（插装层里这两类事件意味着"执行流到了这里"）。

```python
candidate = trace.first_write_then_execute(0x140000000, 0x140100000)
# Candidate(sequence=..., address=..., kind=..., reason="0x... 在序号 ... 处被写入后，
#                                                执行流在 0x... 被命中")
```

**注意 `reason` 字段**：它把判定依据写成一句人话。
分析工具的输出如果不能让第二个人复核，那它就不是分析而是断言。

## 一个完整的脱壳复盘流程

```text
1. 先看完整性
   summary.dropped == 0 且 summary.truncated == False ?
     否 → 结论必须附带"轨迹不完整"这一前提（工具会自己提示）

2. 看外壳在干什么
   trace.slice(...) / 类型分布 → 找到改内存属性、创建线程、读写文件的模式

3. 定位 OEP 候选
   first_write_then_execute(代码段范围) → 拿到候选地址与依据

4. 交叉验证
   在该地址附近 slice 出内存写入 → 确认写入的内容像不像 PE 头
   （用 ev.args[1]/ev.args[2] 或在该点之前 snapshot 的内存）

5. 确认后再 dump
   * 只有在 3、4 都成立时才值得花代价；
   * dump 出来要验证能不能加载（这是唯一可靠的验收）
```

第 5 步是刻意的：**先验证再动手**。在徒手对抗里，"先 dump 一堆再慢慢挑"
会浪费大量时间，而且容易在错误的地址上建立信心。

## 尚未实现（M2/M3）

| 能力 | 状态 | 备注 |
|------|------|------|
| 反向切片（从某点回溯影响它的写入） | ⏳ | 需要先建立"地址 → 写入者"的反向索引 |
| 字节级污点传播 | ⏳ | 需要携带偏移与长度 |
| 调用图重建 | ⏳ | 需要 `reg_write` 与栈事件配合 |
| 多轨迹对比 | ⏳ | 同一目标两次运行的差分，很适合发现"哪次行为不同" |
| 工作台可视化 | ⏳ | `apps/desktop` |

在实现它们之前，请把现有的三个原语当作"辅助定位"，而不是"自动化结论"。
