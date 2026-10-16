"""离线重放与深度分析。

# 为什么离线这一半必须存在

在线插装受两个约束：**不能拖慢目标**、**不能在目标进程里分配大内存**。
所以在线侧只做"采集 + 折叠 + 落盘"，一切需要反复扫描、反复试错的推理
都放到 `.pbt` 上做。这也让分析结论**可复现**：同一份轨迹，换个算法重新跑，
结论应该一致；不一致时至少能定位到是算法变了还是样本变了。

本模块提供三件在对抗场景里最常用的事：

1. [`Trace.slice`]：按地址区间筛事件（"谁动过这块内存"）；
2. [`Trace.taint_forward`]：从若干来源地址出发的**前向污点传播**
   （结构化近似：只看写入/读取地址，不做字节级追踪）；
3. [`Trace.first_write_then_execute`]：定位"某块内存被改写后又被执行"的时刻，
   也就是脱壳里的 OEP 候选点。

三条都是**启发式**，且都在文档里写明了失效条件。把它们称为"启发式"而不是
"算法"是刻意的：分析者必须知道结论有多强。
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path

from pb.trace.event import Event, EventKind
from pb.trace.fold import iter_expanded
from pb.trace.pbt import PbtReader, Summary


@dataclass(slots=True)
class Candidate:
    """一个"候选点"（例如 OEP 候选）。"""

    sequence: int
    address: int
    kind: EventKind
    reason: str


@dataclass(slots=True)
class Trace:
    """一份已加载的轨迹。

    记录（records）是文件里的原始形态；事件（events）是展开后的时间线。
    默认**不展开**：一份 5 分钟的 VMP 轨迹展开后可能是几十亿条事件，
    没人应该在没有明确目的的情况下把它物化到内存里。
    """

    records: list[Event]
    summary: Summary | None = None
    meta: dict[str, object] = field(default_factory=dict)
    audit_lines: list[str] = field(default_factory=list)
    path: Path | None = None

    @property
    def record_count(self) -> int:
        """文件里的记录条数。"""
        return len(self.records)

    def expanded_count(self) -> int:
        """展开后的事件条数（流式统计，不物化）。"""
        return sum(1 for _ in self.events())

    def events(self) -> Iterator[Event]:
        """流式展开事件（内存占用与记录数成正比，与事件数无关）。"""
        yield from iter_expanded(self.records)

    def slice(self, low: int, high: int, *, limit: int | None = None) -> list[Event]:
        """按地址区间 ``[low, high)`` 筛事件（限定在已展开的事件上）。"""
        out: list[Event] = []
        for event in self.events():
            if low <= event.address < high:
                out.append(event)
                if limit is not None and len(out) >= limit:
                    break
        return out

    def taint_forward(self, sources: list[int]) -> dict[int, int]:
        """前向污点传播（结构化近似）。

        规则：某个被标记的页被**读入**（`MemWrite` 的 `args[0]` 是数据来源地址）
        并写到新地址时，新地址也被标记，深度 +1。

        近似之处（必须知道，否则会把线索当证据）：
        * 不做字节级偏移追踪——同 4 KiB 页内偏移被当成同一个地址；
        * 不含算术传播（``lea rax, [rbx+8]`` 这类地址计算看不到）；
        * 依赖事件里带上了"数据来源地址"，而这只在部分 Hook 时机下可得；
        * `MemRead` 默认不订阅，因此"读"这一侧天然是缺失的。

        结论应当当作"值得看的线索"，而不是"证据"。
        """
        page = 1 << 12
        tainted: dict[int, int] = {source // page * page: 0 for source in sources}
        for event in self.events():
            if event.kind != EventKind.MEM_WRITE:
                continue
            origin = event.arg(0) // page * page
            target = event.address // page * page
            if origin in tainted and target not in tainted:
                tainted[target] = tainted[origin] + 1
        return tainted

    def first_write_then_execute(
        self, low: int, high: int, *, watch_from: int = 0
    ) -> Candidate | None:
        """在 ``[low, high)`` 内找"先被写入、随后被执行流到达"的第一个点。

        判定依据：区间内出现 `MemWrite` 之后，出现 `BreakpointHit` 或
        `PatternHit`（这两类事件在插装层意味着"执行流到了这里"）。
        这是脱壳里定位 OEP 的最直接代理指标。
        """
        written: set[int] = set()
        for event in self.events():
            if event.sequence < watch_from:
                continue
            if not (low <= event.address < high):
                continue
            if event.kind == EventKind.MEM_WRITE:
                written.add(event.address)
                continue
            if event.kind in (EventKind.BREAKPOINT_HIT, EventKind.PATTERN_HIT) and written:
                for address in sorted(written):
                    return Candidate(
                        sequence=event.sequence,
                        address=address,
                        kind=event.kind,
                        reason=(
                            f"0x{address:X} 在序号 {event.sequence} 处被写入后，"
                            f"执行流在 0x{event.address:X} 被命中"
                        ),
                    )
        return None

    def timeline(self, *, kinds: set[EventKind] | None = None, limit: int = 40) -> list[str]:
        """可读时间线（CLI 与人工复核用）。"""
        lines: list[str] = []
        for event in self.events():
            if kinds is not None and event.kind not in kinds:
                continue
            lines.append(str(event))
            if len(lines) >= limit:
                break
        return lines


def load(path: str | Path) -> Trace:
    """从文件加载轨迹。"""
    path = Path(path)
    with PbtReader(path) as reader:
        records = reader.read_all()
        summary = reader.summary
        meta_text = reader.meta_text
        audit = reader.audit_text
    meta: dict[str, object] = {}
    if meta_text.strip():
        try:
            parsed = json.loads(meta_text)
            if isinstance(parsed, dict):
                meta = parsed
        except json.JSONDecodeError:
            meta = {"_raw": meta_text.strip()}
    return Trace(
        records=records,
        summary=summary,
        meta=meta,
        audit_lines=[line for line in audit.splitlines() if line.strip()],
        path=path,
    )
