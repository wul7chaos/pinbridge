"""varint / zigzag 编解码测试。"""

from __future__ import annotations

import pytest

from pb.trace.varint import (
    VarintError,
    read_u64,
    read_zigzag,
    try_read_u64,
    write_u64,
    write_zigzag,
)

U64_BOUNDARIES = [0, 1, 0x7F, 0x80, 0x3FFF, 0x4000, 0xFFFF_FFFF, (1 << 64) - 1]
I64_BOUNDARIES = [0, -1, 1, -64, 63, -65, -(1 << 31), -(1 << 63), (1 << 63) - 1]


@pytest.mark.parametrize("value", U64_BOUNDARIES)
def test_u64_roundtrip(value: int) -> None:
    buf = bytearray()
    write_u64(buf, value)
    decoded, offset = read_u64(bytes(buf))
    assert decoded == value
    assert offset == len(buf)


@pytest.mark.parametrize("value", I64_BOUNDARIES)
def test_zigzag_roundtrip(value: int) -> None:
    buf = bytearray()
    write_zigzag(buf, value)
    decoded, offset = read_zigzag(bytes(buf))
    assert decoded == value
    assert offset == len(buf)


def test_small_signed_values_cost_one_byte() -> None:
    """zigzag 的意义：小负数和小正数一样只花 1 字节。

    内存读写地址是双向跳变的（+0x40 与 -0x40 一样常见），
    如果按"符号 + 绝对值"编码，负数会白花一个字节。
    """
    for value in (-64, 63):
        buf = bytearray()
        write_zigzag(buf, value)
        assert len(buf) == 1


def test_truncated_varint_raises() -> None:
    with pytest.raises(VarintError):
        read_u64(b"\x80\x80")
    with pytest.raises(VarintError):
        read_u64(b"")


def test_oversized_varint_is_corruption_not_truncation() -> None:
    """增量解码必须把"还没读完"与"文件坏了"分开。

    混成一个错误会让任何一次分片读取都变成疑似损坏。
    """
    assert try_read_u64(b"") is None
    assert try_read_u64(b"\x80") is None
    assert try_read_u64(b"\x80\x01") == (128, 2)
    with pytest.raises(VarintError):
        read_u64(b"\x80" * 12)


def test_encoder_rejects_negative_u64() -> None:
    with pytest.raises(VarintError):
        write_u64(bytearray(), -1)
