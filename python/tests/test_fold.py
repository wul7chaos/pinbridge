"""重复折叠与展开测试。

这些用例与 Rust 侧 `crates/pinbridge-trace/src/fold.rs` 的测试**一一对应**：
同一个输入、同一个预期输出。两侧任一实现偏离规范都会失败。
"""

from __future__ import annotations

import pytest

from pb.trace.event import Event, EventKind
from pb.trace.fold import FoldError, RepeatFolder, expand_records, fold_events, iter_expanded


def identical_run(
    start: int, count: int, *, thread: int = 100, address: int = 0x7FF6_0000_2000
) -> list[Event]:
    """构造一段"载荷完全相同、序号连续"的事件。"""
    return [
        Event(
            start + offset,
            EventKind.SYSCALL_ENTER,
            thread,
            address,
            [0x40, 0x7FF6_0000_2300, 0, 0, 0, 0, 0, 0],
        )
        for offset in range(count)
    ]


def test_long_run_collapses_to_two_records() -> None:
    events = identical_run(1, 1000)
    folded = fold_events(events)
    assert len(folded) == 2, "1000 条相同连续事件应折叠成 2 条记录"
    assert folded[0].sequence == 1
    assert folded[0].kind == EventKind.SYSCALL_ENTER
    assert folded[1].kind == EventKind.REPEAT
    assert folded[1].sequence == 1000, "标记落在段内最后一条的序号上"
    assert folded[1].arg(0) == 999, "折叠条数不含代表事件"
    assert folded[1].arg(1) == int(EventKind.SYSCALL_ENTER)
    assert folded[1].address == 0x7FF6_0000_2000


def test_fold_then_expand_is_lossless() -> None:
    events = identical_run(1, 500)
    assert expand_records(fold_events(events)) == events


def test_non_contiguous_sequence_breaks_the_run() -> None:
    """序号跳变（目标切线程等）即使载荷相同也不能折叠。"""
    events = identical_run(1, 5)
    for index in range(2, 5):
        events[index].sequence = 1 + index * 2
    folded = fold_events(events)
    assert len(folded) == 5, "序号不连续时必须逐条输出"
    assert expand_records(folded) == events


def test_differing_payload_breaks_the_run() -> None:
    events = identical_run(1, 6)
    events[3].args[1] = 0xDEAD
    folded = fold_events(events)
    assert len(folded) == 5, [str(record) for record in folded]
    assert folded[0].sequence == 1
    assert folded[1].kind == EventKind.REPEAT
    assert (folded[1].sequence, folded[1].arg(0)) == (3, 2)
    assert folded[2].sequence == 4, "差异事件自身作为新段代表被保留"
    assert folded[3].sequence == 5
    assert (folded[4].sequence, folded[4].arg(0)) == (6, 1)
    assert expand_records(folded) == events


def test_single_event_produces_no_marker() -> None:
    events = identical_run(7, 1)
    folded = fold_events(events)
    assert folded == events


def test_outcome_and_cumulative_counter() -> None:
    folder = RepeatFolder()
    events = identical_run(1, 3)
    assert len(folder.push(events[0])) == 1, "新段代表立即输出"
    assert folder.push(events[1]) == [], "被折叠的条数不产生输出"
    other = Event(3, EventKind.BREAKPOINT_HIT, 100, 0x7FF6_0000_2000)
    emitted = folder.push(other)
    assert len(emitted) == 2, "先出折叠标记，再出新段代表"
    assert folder.folded_total == 1


def test_non_monotonic_input_is_rejected() -> None:
    folder = RepeatFolder()
    folder.push(Event(5, EventKind.STATE_CHANGE, 1, 0))
    with pytest.raises(FoldError):
        folder.push(Event(5, EventKind.STATE_CHANGE, 1, 0))
    with pytest.raises(FoldError):
        folder.push(Event(1, EventKind.STATE_CHANGE, 1, 0))


def test_strict_expansion_rejects_inconsistent_marker() -> None:
    """自相矛盾的标记必须被拒绝：取证场景不能接受"看起来完整"的轨迹。"""
    events = identical_run(1, 2)
    bogus = Event.repeat_marker(99, 5, events[0])
    with pytest.raises(FoldError):
        expand_records([events[0], bogus])
    # 宽松模式仍然可用（用于交互式探索）
    assert len(expand_records([events[0], bogus], strict=False)) == 6


def test_isolated_marker_is_rejected_in_strict_mode() -> None:
    marker = Event.repeat_marker(10, 9, identical_run(1, 1)[0])
    with pytest.raises(FoldError):
        expand_records([marker])
    assert expand_records([marker], strict=False) == []


def test_streaming_expansion_matches_batch_expansion() -> None:
    events = identical_run(1, 300) + [Event(301, EventKind.BREAKPOINT_HIT, 100, 0x1400_01000)]
    records = fold_events(events)
    assert list(iter_expanded(records)) == expand_records(records) == events
