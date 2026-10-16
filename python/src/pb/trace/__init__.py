"""轨迹层：事件模型、折叠压缩、`.pbt` 编解码与离线重放。"""

from __future__ import annotations

from pb.trace.event import Event, EventKind
from pb.trace.fold import FoldError, RepeatFolder, expand_records, fold_events, iter_expanded
from pb.trace.pbt import (
    CHUNK_AUDIT,
    CHUNK_END,
    CHUNK_EVENT,
    CHUNK_META,
    CHUNK_SUMMARY,
    FLAG_DELTA_ENCODED,
    FLAG_REPEAT_FOLDED,
    FLAG_TRUNCATED,
    PbtError,
    PbtReader,
    PbtWriter,
    Summary,
)

__all__ = [
    "CHUNK_AUDIT",
    "CHUNK_END",
    "CHUNK_EVENT",
    "CHUNK_META",
    "CHUNK_SUMMARY",
    "FLAG_DELTA_ENCODED",
    "FLAG_REPEAT_FOLDED",
    "FLAG_TRUNCATED",
    "Event",
    "EventKind",
    "FoldError",
    "PbtError",
    "PbtReader",
    "PbtWriter",
    "RepeatFolder",
    "Summary",
    "expand_records",
    "fold_events",
    "iter_expanded",
]
