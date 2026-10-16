"""重复折叠与展开（与 `pinbridge_trace::fold` 等价）。

折叠规则（`docs/design/trace-compression.md` 有完整推导）：

1. **序号连续**：``incoming.sequence == representative.sequence + folded + 1``
2. **载荷相同**：[`Event.same_payload`]（不比序号）

满足两条就把事件折进当前段，段尾写一个 ``REPEAT`` 标记。
这个变换是**可逆**的：[`expand_records`] 能把标记还原成原始事件序列。
"""

from __future__ import annotations

from typing import Iterable, Iterator

from pb.trace.event import Event, EventKind


class FoldError(ValueError):
    """折叠标记自相矛盾（轨迹被改写、拼接，或录制端有 bug）。"""


class RepeatFolder:
    """有状态折叠器。输入必须按序号严格递增。"""

    __slots__ = ("_representative", "_folded", "_folded_total", "_last_input")

    def __init__(self) -> None:
        self._representative: Event | None = None
        self._folded = 0
        self._folded_total = 0
        self._last_input = 0

    @property
    def folded_total(self) -> int:
        """累计折叠掉的条数（会话 summary 里的 `repeat_folded` 取自这里）。"""
        return self._folded_total

    def push(self, event: Event) -> list[Event]:
        """喂入一条事件，返回本次**新增**的输出记录（0~2 条）。

        这里返回列表而不是回调：Python 侧没有"热路径"约束，
        可读性优先（Rust 侧用回调是因为每秒百万次的分配开销）。
        """
        # 序号从 1 开始（0 保留给"未初始化"），因此严格递增即可覆盖全部非法输入
        if event.sequence <= self._last_input:
            raise FoldError(
                f"事件序号必须严格递增：{self._last_input} 之后收到 {event.sequence}"
            )
        self._last_input = event.sequence
        emitted: list[Event] = []

        representative = self._representative
        if representative is None:
            self._representative = event
            emitted.append(event)
            return emitted

        expected = representative.sequence + self._folded + 1
        if expected == event.sequence and representative.same_payload(event):
            self._folded += 1
            self._folded_total += 1
            return emitted

        emitted.extend(self._flush())
        self._representative = event
        emitted.append(event)
        return emitted

    def finish(self) -> list[Event]:
        """结束当前段。**必须调用**，否则最后一段的折叠计数会丢。"""
        return self._flush()

    def _flush(self) -> list[Event]:
        representative = self._representative
        self._representative = None
        if representative is None or self._folded == 0:
            self._folded = 0
            return []
        marker = Event.repeat_marker(
            representative.sequence + self._folded, self._folded, representative
        )
        self._folded = 0
        return [marker]

    def fold(self, events: Iterable[Event]) -> list[Event]:
        """一次性折叠整个序列（便捷入口）。"""
        out: list[Event] = []
        for event in events:
            out.extend(self.push(event))
        out.extend(self.finish())
        return out


def fold_events(events: Iterable[Event]) -> list[Event]:
    """把事件序列折叠成记录序列。"""
    return RepeatFolder().fold(events)


def expand_records(records: Iterable[Event], *, strict: bool = True) -> list[Event]:
    """把记录序列还原成原始事件序列。

    `strict=True`（默认）时会校验折叠标记的自洽性：
    标记序号必须等于"上一条事件序号 + 折叠条数"。
    取证场景必须用严格模式——一份自相矛盾的轨迹如果被静默接受，
    结论会看起来完整但实际错误。
    """
    out: list[Event] = []
    for record in records:
        if record.kind != EventKind.REPEAT:
            out.append(record)
            continue
        if not out:
            if strict:
                raise FoldError("轨迹以折叠标记开头，缺少代表事件")
            continue
        previous = out[-1]
        count = record.arg(0)
        if strict:
            if count == 0:
                raise FoldError("折叠标记的条数为 0，应当直接省略该标记")
            expected = previous.sequence + count
            if expected != record.sequence:
                raise FoldError(
                    f"折叠标记自相矛盾：上一条序号 {previous.sequence} + 折叠 {count} "
                    f"!= 标记序号 {record.sequence}"
                )
        for offset in range(1, count + 1):
            clone = Event(
                previous.sequence + offset,
                previous.kind,
                previous.thread_id,
                previous.address,
                list(previous.args),
            )
            out.append(clone)
    return out


def iter_expanded(records: Iterable[Event]) -> Iterator[Event]:
    """流式展开（轨迹很大时避免一次性物化整个列表）。"""
    previous: Event | None = None
    for record in records:
        if record.kind != EventKind.REPEAT:
            previous = record
            yield record
            continue
        if previous is None:
            continue
        for offset in range(1, record.arg(0) + 1):
            yield Event(
                previous.sequence + offset,
                previous.kind,
                previous.thread_id,
                previous.address,
                list(previous.args),
            )
