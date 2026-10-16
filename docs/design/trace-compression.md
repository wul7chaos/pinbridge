# 轨迹压缩：重复折叠

## 问题

VMP 类目标的外壳会把关键循环展开成"**同一条指令 + 同一个地址 + 同样的参数**"
重复执行几十万到上亿次（变形、解密、校验、反调试心跳）。

按原样录制：

* 一条 5 分钟的会话可以产出**几十 GB**轨迹；
* 其中 99.99% 是同一个字节序列，对人类没有信息量；
* 对 LLM 更是纯 token 灾难——而且会挤掉真正有信息量的那几百条事件。

## 折叠规则

一条事件只有**同时满足**下面两点，才会被折进当前重复段：

1. **序号连续**：`incoming.sequence == representative.sequence + folded + 1`
2. **载荷相同**（`Event::same_payload`）：类型、线程、地址、`args[0..8]` 全等。
   **不比较序号**。

为什么"不比较序号"是核心：序号正是我们要折叠掉的信息。
同一条指令重复执行 100 万次时，只有序号在变，其余完全相同。
（本仓库参考文章里的 `same_payload` 也是这个语义。）

## 折叠产物

段的**第一条原样输出**（保留完整载荷，供重放时复制），
段尾写一个 `REPEAT` 标记：

| 字段 | 含义 |
|------|------|
| `kind` | `EVENT_REPEAT`（判别值 12） |
| `sequence` | 该重复段**最后一条**事件的序号 |
| `args[0]` | 被折叠掉的条数（**不含**第一次出现的那条） |
| `args[1]` | 重复段的原始事件类型判别值 |
| `thread_id` / `address` | 取自本段 |

于是"1000 条相同连续事件"变成 2 条记录。

### 与参考实现的对照

参考文章给出的片段是：

```rust
let expected = previous.sequence.saturating_add(pending_repeats).saturating_add(1);
if expected == event.sequence && same_payload(&previous, &event) {
    pending = Some(previous);
    pending_repeats = pending_repeats.saturating_add(1);
}
```

```rust
if repeat_count > 0 {
    let repeat = Event {
        sequence: event.sequence.saturating_add(repeat_count),
        kind: EVENT_REPEAT,
        thread_id: event.thread_id,
        address: event.address,
        arg0: repeat_count,
        arg1: event.kind as u64,
        ..Event::EMPTY
    };
    write_record(scratch, &repeat);
}
```

本实现把它形式化为：**标记落在段的最后一个序号上，`arg0` 是被折掉的条数**。
当片段里的 `event` 指"该段的代表事件"时，两者的 `sequence` 表达式形式一致
（代表序号 + 折叠条数 = 段尾序号）。我们把它明确写成规范并配了可逆性测试，
因为"靠注释约定"的压缩格式迟早会分叉。

## 可逆性（硬要求）

压缩后的轨迹如果还原不出原时间线，就没有资格被用作分析证据。
展开规则：

```text
遇到 REPEAT 标记：
    取上一条非标记事件的载荷
    依次生成 args[0] 条事件，序号从 上一条序号+1 递增到 标记序号
    标记自身不产生事件
```

三重保障：

1. **Rust 侧**：`fold_then_expand_is_lossless` 等用例；
2. **Python 侧**：同一组用例（`python/tests/test_fold.py`）；
3. **端到端**：`pinbridge demo` 把 4069 条事件折叠成 71 条记录再从磁盘读回，
   断言"71 条记录无损还原为 4069 条事件"。

## 严格模式：拒绝自相矛盾的轨迹

`expand_records_checked`（Python 侧 `strict=True`，默认开启）会校验：

* 标记序号必须等于"上一条事件序号 + 折叠条数"；
* 折叠条数不能是 0（应当是直接省略标记）；
* 轨迹不能以标记开头。

一份自相矛盾的轨迹意味着它被改写、拼接，或录制端有 bug。
**在这里失败比对着一份"看起来完整"的轨迹下结论要便宜得多。**

交互式探索可以用宽松模式（`strict=False`），但取证场景必须用严格模式。

## 其余三档压缩

| 档位 | 做法 | 收益 | 状态 |
|------|------|------|------|
| `none` | 原样写每条事件 | — | ✅ |
| `repeat` | 上面的重复折叠 | VMP 场景常见 10²–10⁴ 倍 | ✅ |
| `repeat+delta` | 折叠 + 记录内 delta/varint/zigzag | 再省 50%–80% | ✅（默认） |
| 寄存器去重 | 寄存器值未变化时不记录 | 中等 | ✅（`argc` 省略末尾未变参数） |
| 稀疏采样（时间窗） | 按时间窗只留代表性事件 | 高 | ❌ 不做（会丢证据，与本项目立场冲突） |

`repeat+delta` 的具体编码见 [../specs/pbt-format.md](../specs/pbt-format.md)。

## 怎么知道压缩是不是把有用的东西压掉了

看 `SUMMARY` 段里的三个数字：

| 字段 | 含义 | 什么时候要警惕 |
|------|------|----------------|
| `records` | 落盘记录数 | 异常小时可能是模板太宽泛，把不同行为合并了 |
| `events` | 展开后事件数 | 与你的直觉差距过大时，先确认订阅集 |
| `repeat_folded` | 被折掉的条数 | 折叠率 > 99.9% 时，说明这段几乎全是同一个循环 |
| `dropped` | 背压丢弃数 | **非 0 即代表轨迹不完整**，结论必须打折 |
| `truncated` | 是否异常中断 | `true` 时尾部信息缺失 |

`fold_ratio()` 就是 `repeat_folded / (records + repeat_folded)`。

**注意**：折叠统计是在**落盘侧**做的。Hub 的 `query` 是在窗口切片上折叠的，
如果一段重复横跨窗口边界，边界处那条的折叠计数不完整。
需要精确统计时必须读 `.pbt`。
