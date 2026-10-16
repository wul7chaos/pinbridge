""".pbt 轨迹文件的 Python 编解码器。

规范正文：`docs/specs/pbt-format.md`。本文件的职责只有一件事：
**成为规范的忠实转写**。任何"看起来更聪明"的解析（自动纠正、宽松容忍）都必须
先写进规范，否则 Rust 与 Python 两侧就会悄悄分叉。

容器结构：

.. code-block:: text

    File  := "PBT1"(4) | version u16 | flags u16 | Chunk*
    Chunk := tag u8 | length u32 | payload[length]

记录结构（delta + varint + zigzag）：

.. code-block:: text

    record := varint(seq_delta) | u8 kind | varint(thread_id)
            | zigzag(addr_delta) | u8 argc | argc × zigzag(arg_delta[i])

`argc` 取"与上一条记录相比最后一个发生变化的参数下标 + 1"，
末尾未变化的参数整段省略（同 Rust 侧，也同参考文章里"寄存器没变就不记录"的思路）。
"""

from __future__ import annotations

import json
import struct
from collections.abc import Iterable, Iterator
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import BinaryIO, Protocol

from pb import FORMAT_VERSION, MAGIC, MAX_ARGS
from pb.trace.event import Event, EventKind
from pb.trace.varint import try_read_u64, try_read_zigzag, write_u64, write_zigzag


class ReadableSource(Protocol):
    """`PbtReader` 真正需要的最小接口。

    刻意**不去满足 `BinaryIO`**：那要求实现十余个方法（`readinto`、`seekable`、
    `writable`…），而这里只用到两个。为了通过类型检查而实现一堆用不到的方法，
    是典型的负担转移；用一个两方法的 Protocol 反而把"我们到底依赖了什么"讲清楚了。

    参数名用 `n`（与 `BinaryIO.read` 一致），这样真实的文件对象、
    `io.BytesIO` 与本模块的 `_BytesIo` 都结构化兼容。
    """

    def read(self, n: int) -> bytes: ...  # noqa: E704

    def close(self) -> None: ...  # noqa: E704


# --- 段 tag（0x00..=0x3F 为本格式保留，0x40..=0x7E 给实验扩展，0x7F 结束）---
CHUNK_META = 0x00
CHUNK_EVENT = 0x01
CHUNK_AUDIT = 0x02
CHUNK_SUMMARY = 0x03
CHUNK_END = 0x7F

# --- header 标志位 ---
FLAG_REPEAT_FOLDED = 1 << 0
FLAG_DELTA_ENCODED = 1 << 1
FLAG_TRUNCATED = 1 << 2

HEADER_LEN = 8
DEFAULT_CHUNK_LIMIT = 64 * 1024
#: 单个 chunk 的负载上限。防"坏文件声称自己有 4 GiB 负载"式的内存放大。
MAX_CHUNK_BYTES = 256 * 1024 * 1024


class PbtError(ValueError):
    """轨迹文件非法（magic/版本不对、截断、记录自相矛盾）。"""


@dataclass(slots=True)
class Summary:
    """轨迹汇总。人/AI 判断"这份轨迹能不能用来下结论"的第一手依据。"""

    records: int = 0
    events: int = 0
    repeat_folded: int = 0
    dropped: int = 0
    first_sequence: int = 0
    last_sequence: int = 0
    truncated: bool = False

    @property
    def fold_ratio(self) -> float:
        """折叠率（0.0 ~ 1.0）。"""
        total = self.records + self.repeat_folded
        return self.repeat_folded / total if total else 0.0

    def to_json(self) -> str:
        """转 JSON（与 Rust `Summary::to_json` 字段名一致）。"""
        return json.dumps(asdict(self), ensure_ascii=False, separators=(",", ":"))

    @classmethod
    def from_json(cls, text: str) -> Summary:
        """从 JSON 解析。未知字段忽略，缺失字段取默认值。"""
        try:
            raw = json.loads(text)
        except json.JSONDecodeError as error:
            raise PbtError(f"summary 不是合法 JSON: {error}") from error
        if not isinstance(raw, dict):
            raise PbtError("summary 顶层必须是对象")
        known = set(cls.__dataclass_fields__)
        return cls(**{key: value for key, value in raw.items() if key in known})


def _encode_chunk(tag: int, payload: bytes) -> bytes:
    return bytes([tag]) + struct.pack("<I", len(payload)) + payload


class PbtWriter:
    """轨迹写入器。用法：`write_record` 若干次之后 `finish()`。"""

    def __init__(
        self,
        sink: BinaryIO,
        *,
        flags: int = 0,
        chunk_limit: int = DEFAULT_CHUNK_LIMIT,
    ) -> None:
        self._sink = sink
        self._event_buf = bytearray()
        self._chunk_limit = max(1, chunk_limit)
        self._flags = flags | FLAG_DELTA_ENCODED
        self._summary = Summary()
        self._last_sequence = 0
        self._last_address = 0
        self._last_args = [0] * MAX_ARGS
        self._finished = False
        sink.write(MAGIC + struct.pack("<HH", FORMAT_VERSION, self._flags))

    # ------------------------------------------------------------------ 段

    def write_chunk(self, tag: int, payload: bytes) -> None:
        """写一个任意段。"""
        if len(payload) > 0xFFFF_FFFF:
            raise PbtError(f"chunk 负载过大: {len(payload)} 字节")
        self._sink.write(_encode_chunk(tag, payload))

    def meta(self, text: str) -> None:
        """写元数据段（JSON 文本）。"""
        self.write_chunk(CHUNK_META, text.encode("utf-8"))

    def audit_line(self, line: str) -> None:
        """写一行审计记录。"""
        self.write_chunk(CHUNK_AUDIT, (line + "\n").encode("utf-8"))

    # ---------------------------------------------------------------- 记录

    def write_record(self, event: Event) -> None:
        """写一条记录（普通事件或折叠标记）。序号必须严格递增。"""
        if self._finished:
            raise PbtError("写入器已 finish，不能继续写入")
        if event.sequence <= self._last_sequence:
            raise PbtError(
                f"事件序号必须严格单调递增: 上一条 {self._last_sequence} 之后收到 {event.sequence}"
            )
        self._encode_into(event.sequence, event.kind, event.thread_id, event.address, event.args)
        self._summary.records += 1
        if event.kind == EventKind.REPEAT:
            folded = event.arg(0)
            self._summary.repeat_folded += folded
            self._summary.events += folded
        else:
            self._summary.events += 1
        if self._summary.first_sequence == 0:
            self._summary.first_sequence = event.sequence
        self._summary.last_sequence = event.sequence
        if len(self._event_buf) >= self._chunk_limit:
            self._flush_events()

    def write_records(self, events: Iterable[Event]) -> None:
        """批量写。"""
        for event in events:
            self.write_record(event)

    def note_dropped(self, dropped: int) -> None:
        """登记被背压丢弃的事件数（必须如实传入）。"""
        self._summary.dropped += dropped

    @property
    def summary(self) -> Summary:
        """当前累计汇总。"""
        return self._summary

    @property
    def flags(self) -> int:
        """写入文件头时使用的标志位。"""
        return self._flags

    def finish(self) -> Summary:
        """收尾：冲刷缓冲、写 SUMMARY 与 END。"""
        self._flush_events()
        self.write_chunk(CHUNK_SUMMARY, self._summary.to_json().encode("utf-8"))
        self.write_chunk(CHUNK_END, b"")
        self._sink.flush()
        self._finished = True
        return self._summary

    # ------------------------------------------------------------ 内部实现

    def _encode_into(
        self,
        sequence: int,
        kind: EventKind,
        thread_id: int,
        address: int,
        args: list[int],
    ) -> None:
        buf = self._event_buf
        write_u64(buf, sequence - self._last_sequence)
        buf.append(int(kind))
        write_u64(buf, thread_id)
        write_zigzag(buf, _wrap_signed(address - self._last_address))
        argc = 0
        for index in range(MAX_ARGS):
            if args[index] != self._last_args[index]:
                argc = index + 1
        buf.append(argc)
        for index in range(argc):
            write_zigzag(buf, _wrap_signed(args[index] - self._last_args[index]))
        self._last_sequence = sequence
        self._last_address = address
        self._last_args = list(args)

    def _flush_events(self) -> None:
        if not self._event_buf:
            return
        self.write_chunk(CHUNK_EVENT, bytes(self._event_buf))
        self._event_buf.clear()


class PbtReader:
    """流式轨迹读取器。

    既接受 `bytes`，也接受已打开的二进制文件对象（大文件走文件对象）。
    """

    def __init__(self, source: bytes | bytearray | BinaryIO | str | Path) -> None:
        if isinstance(source, (bytes, bytearray)):
            self._source: ReadableSource = _BytesIo(source)
            self._owns_source = False
        elif isinstance(source, (str, Path)):
            # 这里刻意不使用 `with`：句柄的生命周期必须由 PbtReader 自己持有，
            # 因为它要在多次 `records()` 调用之间保持打开。__exit__/close 负责关闭。
            self._source = Path(source).open("rb")  # noqa: SIM115
            self._owns_source = True
        else:
            self._source = source
            self._owns_source = False
        self._buf = bytearray()
        self._pos = 0
        self._eof = False
        self._end_seen = False
        self.meta_text = ""
        self.audit_text = ""
        self.summary_text = ""
        self._last_sequence = 0
        self._last_address = 0
        self._last_args = [0] * MAX_ARGS
        self.records_read = 0

        header = self._read_exact(HEADER_LEN)
        if len(header) < HEADER_LEN:
            raise PbtError("文件为空或过短，不是有效的 .pbt")
        if header[:4] != MAGIC:
            raise PbtError(f"文件头不是 PBT1 magic，实际: {header[:4]!r}")
        version, flags = struct.unpack("<HH", header[4:8])
        if version != FORMAT_VERSION:
            raise PbtError(f"不支持的轨迹版本 {version}（本实现支持 {FORMAT_VERSION}）")
        self.version = version
        self.flags = flags

    def __enter__(self) -> PbtReader:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def close(self) -> None:
        """若由本对象打开了文件，则关闭它。"""
        if self._owns_source:
            self._source.close()

    @property
    def summary(self) -> Summary | None:
        """SUMMARY 段解析结果（缺失时为 `None`，说明轨迹被中断）。"""
        return Summary.from_json(self.summary_text) if self.summary_text else None

    def __iter__(self) -> Iterator[Event]:
        return self.records()

    def records(self) -> Iterator[Event]:
        """逐条产出记录（折叠标记原样返回，不展开）。"""
        while True:
            event = self._next_record()
            if event is None:
                return
            yield event

    def read_all(self) -> list[Event]:
        """一次性读完所有记录。"""
        return list(self.records())

    # ------------------------------------------------------------ 内部实现

    def _next_record(self) -> Event | None:
        while True:
            decoded = self._try_decode()
            if decoded is not None:
                event, used = decoded
                self._pos += used
                self._last_sequence = event.sequence
                self._last_address = event.address
                self._last_args = list(event.args)
                self.records_read += 1
                return event
            if not self._fill():
                if self._pos < len(self._buf):
                    raise PbtError(
                        f"轨迹在第 {self.records_read} 条记录之后被截断"
                        f"（剩 {len(self._buf) - self._pos} 字节无法解出完整记录）"
                    )
                return None

    def _try_decode(self) -> tuple[Event, int] | None:
        data = self._buf
        cursor = self._pos

        step = try_read_u64(data, cursor)
        if step is None:
            return None
        seq_delta, cursor = step

        if cursor >= len(data):
            return None
        kind = EventKind.from_code(data[cursor])
        cursor += 1

        step = try_read_u64(data, cursor)
        if step is None:
            return None
        thread_id, cursor = step
        if thread_id > 0xFFFF_FFFF:
            raise PbtError(f"线程 ID 越界: {thread_id}")

        step = try_read_zigzag(data, cursor)
        if step is None:
            return None
        addr_delta, cursor = step

        if cursor >= len(data):
            return None
        argc = data[cursor]
        cursor += 1
        if argc > MAX_ARGS:
            raise PbtError(f"参数个数越界: {argc}")

        args = list(self._last_args)
        for index in range(argc):
            step = try_read_zigzag(data, cursor)
            if step is None:
                return None
            delta, cursor = step
            args[index] = (self._last_args[index] + delta) & ((1 << 64) - 1)

        event = Event(
            sequence=(self._last_sequence + seq_delta) & ((1 << 64) - 1),
            kind=kind,
            thread_id=thread_id,
            address=(self._last_address + addr_delta) & ((1 << 64) - 1),
            args=args,
        )
        return event, cursor - self._pos

    def _fill(self) -> bool:
        """读取下一个 chunk。返回 True 表示向事件缓冲追加了数据。"""
        if self._end_seen or self._eof:
            return False
        if self._pos:
            del self._buf[: self._pos]
            self._pos = 0
        while True:
            head = self._read_exact(5)
            if not head:
                self._eof = True
                return False
            if len(head) < 5:
                # 读到 1~4 字节就没了：这不是"干净结束"，而是文件被砍断
                raise PbtError("轨迹在 chunk 头中途结束")
            tag = head[0]
            length = struct.unpack("<I", head[1:5])[0]
            if length > MAX_CHUNK_BYTES:
                raise PbtError(f"chunk 长度 {length} 超出上限")
            payload = self._read_exact(length)
            if len(payload) < length:
                raise PbtError("轨迹在 chunk 负载中途结束")
            if tag == CHUNK_EVENT:
                self._buf.extend(payload)
                return True
            if tag == CHUNK_META:
                self.meta_text += payload.decode("utf-8", errors="replace")
            elif tag == CHUNK_AUDIT:
                self.audit_text += payload.decode("utf-8", errors="replace")
            elif tag == CHUNK_SUMMARY:
                self.summary_text += payload.decode("utf-8", errors="replace")
            elif tag == CHUNK_END:
                self._end_seen = True
                return False
            # 未知 tag：按长度跳过（向前兼容的唯一实现方式）

    def _read_exact(self, count: int) -> bytes:
        if count == 0:
            return b""
        chunks: list[bytes] = []
        remaining = count
        while remaining:
            piece = self._source.read(remaining)
            if not piece:
                break
            chunks.append(piece)
            remaining -= len(piece)
        return b"".join(chunks)


def _wrap_signed(value: int) -> int:
    """把差值折算到 i64 范围（与 Rust 的 `wrapping_sub` 语义一致）。"""
    value &= (1 << 64) - 1
    return value - (1 << 64) if value >= (1 << 63) else value


class _BytesIo:
    """极简的只读字节流包装（避免为 bytes 输入引入 io.BytesIO 的额外状态）。"""

    __slots__ = ("_data", "_offset")

    def __init__(self, data: bytes | bytearray) -> None:
        self._data = bytes(data)
        self._offset = 0

    def read(self, n: int) -> bytes:
        piece = self._data[self._offset : self._offset + n]
        self._offset += len(piece)
        return piece

    def close(self) -> None:
        self._data = b""
        self._offset = 0
