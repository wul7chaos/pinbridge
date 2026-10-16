# python/ —— 离线分析侧

与 Rust 侧的关系是**同一份规范的两个实现**：
Rust 负责在线（注入目标、采集、折叠、落盘），Python 负责离线
（解码、重放、污点、OEP 候选、切片）。

## 安装

```bash
cd python
pip install -e ".[dev]"        # 或者用 uv
pytest -q
```

**运行期零依赖**。解析轨迹的代码必须能在任何一台机器上直接跑起来——
每多一个依赖，就多一个"分析现场装不上环境"的风险。

## 用法

```bash
# 体检一份轨迹
pb-replay TRACE.pbt --head 20

# 从某个地址出发做前向污点传播
pb-replay TRACE.pbt --taint 0x7FF600003000

# 找 OEP 候选
pb-replay TRACE.pbt --oep-range 0x140000000-0x140100000
```

```python
from pb.trace.replay import load

trace = load("TRACE.pbt")
print(trace.summary)  # 先看完整性：dropped / truncated
print(trace.expanded_count())  # 展开后的事件数（流式统计，不物化）

# 谁动过这块内存
for ev in trace.slice(0x7FF600003000, 0x7FF600004000, limit=50):
    print(ev)

# 找"写入后被执行"的点（OEP 候选）
candidate = trace.first_write_then_execute(0x140000000, 0x140100000)
print(candidate)
```

## 内存纪律

`Trace.records` 是文件里的原始形状（已被折叠，通常比事件数小几个数量级），
`trace.events()` 是**流式**展开的迭代器。

一份 5 分钟的 VMP 轨迹展开后可能是几十亿条事件——
**不要在没有目的时物化它**（`expanded_count()` 是流式统计的，不会炸内存）。

## 与 Rust 的一致性

| 机制 | 位置 |
|------|------|
| 黄金字节（同一批记录 → 同一条字节流） | `tests/test_pbt.py::test_golden_event_payload_matches_rust` |
| 常量对齐（magic/版本/类型表/能力短名） | `tests/test_rust_parity.py` |
| 端到端互读（Rust 录 → Python 读） | `tests/test_rust_parity.py::test_python_reads_a_trace_written_by_rust` |

最后一条最关键：单测共享常量只能证明"两边都按我以为的规范写了"，
只有互读能证明它们真的互通。**CI 会先 `cargo build` 再跑它**，
避免它被静默跳过。

## 目录

```text
src/pb/
  __init__.py         版本与格式常量（与 Rust 侧必须相同）
  trace/
    varint.py         LEB128 / zigzag（区分"没读完"与"坏了"）
    event.py          Event / EventKind（判别值与 Rust、C 三处一致）
    fold.py           重复折叠与展开（含严格模式）
    pbt.py            .pbt 编解码（chunk 容器 + delta 记录）
    replay.py         Trace / 切片 / 污点 / OEP 候选
    cli.py            pb-replay
tests/
  test_varint.py      边界、截断、超长
  test_fold.py        折叠语义与可逆性（与 Rust 侧用例一一对应）
  test_pbt.py         编解码、黄金字节、防御（截断/未知段/越界）
  test_rust_parity.py 跨语言一致性
```

## 改这里的注意事项

1. 改编码前先读 [../docs/specs/pbt-format.md](../docs/specs/pbt-format.md)；
2. **不做猜测性解析**：未知类型、超界 `argc`、自相矛盾的折叠标记一律抛错；
3. 与 Rust 对应的字段名必须一致（`records` / `events` / `repeat_folded`…）——
   字段名不同会让"对比两侧输出"退化成人工映射，而人工映射必然出错；
4. 涉及启发式的函数要在 docstring 里写明**失效条件**。

详细规范见 [../docs/dev/coding-standards-python.md](../docs/dev/coding-standards-python.md)。
