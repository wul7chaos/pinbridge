""".pbt 编解码测试，含与 Rust 实现共享的**黄金字节**用例。"""

from __future__ import annotations

import io
import struct

import pytest

from pb import FORMAT_VERSION, MAGIC
from pb.trace.event import Event, EventKind
from pb.trace.fold import expand_records, fold_events
from pb.trace.pbt import (
    CHUNK_EVENT,
    FLAG_DELTA_ENCODED,
    FLAG_REPEAT_FOLDED,
    PbtError,
    PbtReader,
    PbtWriter,
    Summary,
)

# 由 Rust 侧 `pinbridge-trace` 的 `golden_event_payload_hex_matches_python` 测试打印，
# 两侧断言同一个常量：任何一边改了编码都会同时失败。
GOLDEN_EVENT_PAYLOAD_HEX = (
    "01016480c08080c0fd3f0180c0808028010364804003ffbe808028808c8180c0fd3f10e6070c640003"
    "ca0ef98b8180c0fd3f0f"
)


def golden_records() -> list[Event]:
    """黄金用例的三条记录（与 Rust 侧同构）。"""
    sample = Event(
        2,
        EventKind.SYSCALL_ENTER,
        100,
        0x7FF6_0000_2000,
        [0x40, 0x7FF6_0000_2300, 8, 0, 0, 0, 0, 0],
    )
    return [
        Event(
            1, EventKind.BREAKPOINT_HIT, 100, 0x7FF6_0000_1000, [0x1400_01000, 0, 0, 0, 0, 0, 0, 0]
        ),
        sample,
        Event.repeat_marker(1000, 997, sample),
    ]


def extract_event_payload(data: bytes) -> bytes:
    """从完整文件字节中取出第一个 EVENT 段的负载。"""
    offset = 8
    while offset + 5 <= len(data):
        tag = data[offset]
        length = struct.unpack("<I", data[offset + 1 : offset + 5])[0]
        offset += 5
        if tag == CHUNK_EVENT:
            return data[offset : offset + length]
        offset += length
    raise AssertionError("文件里没有 EVENT 段")


def write_trace(records: list[Event], *, chunk_limit: int = 64 * 1024, flags: int = 0) -> bytes:
    """把记录写成完整的 .pbt 字节流。"""
    sink = io.BytesIO()
    writer = PbtWriter(sink, flags=flags, chunk_limit=chunk_limit)
    writer.meta('{"tool":"pb-python-tests"}')
    writer.write_records(records)
    writer.audit_line('{"actor":"test","action":"write"}')
    writer.finish()
    return sink.getvalue()


def fixture_events() -> list[Event]:
    """带一段长重复的夹具：3 条普通事件 + 500 条完全相同的 syscall。"""
    events = [
        Event(
            1, EventKind.BREAKPOINT_HIT, 100, 0x7FF6_0000_1000, [0x1400_01000, 0, 0, 0, 0, 0, 0, 0]
        ),
        Event(
            2,
            EventKind.SYSCALL_ENTER,
            100,
            0x7FF6_0000_2000,
            [0x40, 0x7FF6_0000_2300, 8, 0, 0, 0, 0, 0],
        ),
        Event(
            3,
            EventKind.MEM_WRITE,
            100,
            0x7FF6_0000_3000,
            [0x7FF6_0000_3000, 4, 0xDEAD_BEEF, 0, 0, 0, 0, 0],
        ),
    ]
    events.extend(
        Event(
            4 + offset,
            EventKind.SYSCALL_ENTER,
            100,
            0x7FF6_0000_2000,
            [0x40, 0x7FF6_0000_2300, 8, 0, 0, 0, 0, 0],
        )
        for offset in range(500)
    )
    return events


def test_golden_event_payload_matches_rust() -> None:
    """黄金字节：与 Rust 侧的同一断言共享常量。"""
    data = write_trace(golden_records(), chunk_limit=1 << 30)
    payload = extract_event_payload(data)
    assert payload.hex() == GOLDEN_EVENT_PAYLOAD_HEX, (
        "编码与 Rust 侧不一致：这是在跨语言读轨迹时最难排查的一类 bug"
    )


def test_roundtrip_with_folding_is_lossless() -> None:
    events = fixture_events()
    records = fold_events(events)
    assert len(records) < len(events) / 50, "折叠后记录数应大幅下降"

    data = write_trace(records)
    with PbtReader(data) as reader:
        decoded = reader.read_all()
    assert decoded == records, "编解码必须逐位可逆"
    assert expand_records(decoded) == events, "折叠 + 编解码之后必须还原出原始事件序列"


def test_header_and_sections_are_readable() -> None:
    events = fixture_events()
    records = fold_events(events)
    data = write_trace(records, flags=FLAG_REPEAT_FOLDED)
    with PbtReader(data) as reader:
        assert reader.version == FORMAT_VERSION
        assert reader.flags == FLAG_REPEAT_FOLDED | FLAG_DELTA_ENCODED
        assert reader.records_read == 0
        decoded = reader.read_all()
        assert len(decoded) == len(records)
        assert "pb-python-tests" in reader.meta_text
        assert '"action":"write"' in reader.audit_text
        summary = reader.summary
    assert summary is not None
    assert summary.records == len(records)
    assert summary.events == len(events)
    assert summary.repeat_folded == 499
    assert (summary.first_sequence, summary.last_sequence) == (1, 503)
    assert summary.fold_ratio > 0.99
    assert summary.dropped == 0 and summary.truncated is False


def test_records_may_span_chunk_boundaries() -> None:
    """把 chunk 限到 16 字节，强制大量记录跨 chunk 分割。"""
    records = fold_events(fixture_events())
    data = write_trace(records, chunk_limit=16)
    with PbtReader(data) as reader:
        assert reader.read_all() == records


def test_unknown_chunk_is_skipped() -> None:
    """向前兼容的唯一实现方式：未知 tag 按长度跳过。"""
    events = fold_events(fixture_events())
    sink = io.BytesIO()
    writer = PbtWriter(sink)
    writer.write_chunk(0x40, b"future-section-payload")
    writer.write_records(events)
    writer.finish()
    with PbtReader(sink.getvalue()) as reader:
        assert reader.read_all() == events


def test_bad_magic_and_version_are_rejected() -> None:
    data = bytearray(write_trace(fold_events(fixture_events())))
    bad = bytes([ord("X")]) + bytes(data[1:])
    with pytest.raises(PbtError, match="magic"):
        PbtReader(bad)
    version_bumped = bytes(data[:4]) + struct.pack("<H", 99) + bytes(data[6:])
    with pytest.raises(PbtError, match="不支持的轨迹版本"):
        PbtReader(version_bumped)
    with pytest.raises(PbtError):
        PbtReader(b"")


def test_truncated_file_is_reported_not_swallowed() -> None:
    data = write_trace(fold_events(fixture_events()))
    with pytest.raises(PbtError):
        PbtReader(data[: len(data) - 300]).read_all()


def test_empty_trace_is_valid() -> None:
    sink = io.BytesIO()
    summary = PbtWriter(sink).finish()
    assert summary.records == 0
    with PbtReader(sink.getvalue()) as reader:
        assert reader.read_all() == []
        assert reader.summary is not None
        assert reader.summary.records == 0


def test_non_monotonic_sequence_is_rejected_at_write_time() -> None:
    sink = io.BytesIO()
    writer = PbtWriter(sink)
    writer.write_record(Event(10, EventKind.STATE_CHANGE, 1, 0))
    with pytest.raises(PbtError, match="单调递增"):
        writer.write_record(Event(10, EventKind.STATE_CHANGE, 1, 0))
    with pytest.raises(PbtError, match="单调递增"):
        writer.write_record(Event(9, EventKind.STATE_CHANGE, 1, 0))


def test_unknown_event_kind_is_rejected() -> None:
    with pytest.raises(ValueError):
        EventKind.from_code(200)
    with pytest.raises(ValueError):
        EventKind.from_code(0)
    with pytest.raises(ValueError):
        EventKind.from_name("not_a_kind")


def test_reserved_kind_cannot_be_read_from_a_file() -> None:
    """判别值 0 是保留值：出现在文件里必须报错，而不是当成一条无害记录。"""
    sink = io.BytesIO()
    writer = PbtWriter(sink)
    writer._encode_into(1, EventKind.RESERVED, 1, 0, [0] * 8)  # noqa: SLF001 - 刻意构造非法文件
    writer.finish()
    with pytest.raises(ValueError):
        PbtReader(sink.getvalue()).read_all()


def test_summary_json_roundtrip() -> None:
    original = Summary(records=3, events=1000, repeat_folded=997, last_sequence=1000)
    assert Summary.from_json(original.to_json()) == original
    # 未知字段忽略、缺失字段取默认值：向前兼容
    assert Summary.from_json('{"records":1,"future_field":true}').records == 1
    with pytest.raises(PbtError):
        Summary.from_json("not json")


def test_file_object_and_path_inputs_are_supported(tmp_path) -> None:
    events = fold_events(fixture_events())
    path = tmp_path / "trace.pbt"
    with path.open("wb") as handle:
        sink = io.BytesIO(write_trace(events))
        handle.write(sink.getvalue())
    with PbtReader(path) as reader:
        assert reader.read_all() == events
    assert MAGIC in path.read_bytes()[:4]
