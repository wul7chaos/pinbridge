"""varint / zigzag 编解码。

与 `pinbridge_trace::varint` 逐字节等价。
"""

from __future__ import annotations


class VarintError(ValueError):
    """变长整数非法（超长或数据被截断）。"""


def write_u64(out: bytearray, value: int) -> None:
    """写入无符号 LEB128。"""
    if value < 0:
        raise VarintError(f"write_u64 只接受无符号值，收到 {value}")
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return


def write_zigzag(out: bytearray, value: int) -> None:
    """zigzag 映射后写 varint。

    与 Rust 的 ``((value << 1) ^ (value >> 63)) as u64`` 等价：
    Python 的 ``>>`` 对负数同样是算术右移（得 -1），最后按 64 位掩码截断。
    """
    zz = ((value << 1) ^ (value >> 63)) & ((1 << 64) - 1)
    write_u64(out, zz)


def read_u64(data: bytes, offset: int = 0) -> tuple[int, int]:
    """读取无符号 LEB128，返回 ``(值, 新偏移)``。

    超过 10 字节视为损坏（与 Rust 侧一致）：这是解码器唯一必须防御的地方，
    轨迹文件是外部输入。
    """
    value = 0
    shift = 0
    for index in range(offset, offset + 10):
        if index >= len(data):
            raise VarintError(f"varint 在偏移 {offset} 处被截断")
        byte = data[index]
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, index + 1
        shift += 7
    raise VarintError(f"偏移 {offset} 处的 varint 超过 10 字节，文件可能已损坏")


def try_read_u64(data: bytes, offset: int = 0) -> tuple[int, int] | None:
    """增量解码：数据不足时返回 ``None``（而不是抛异常）。

    流式读取 `.pbt` 时必须区分"还没读到"和"文件坏了"。
    """
    value = 0
    shift = 0
    for index in range(offset, offset + 10):
        if index >= len(data):
            return None
        byte = data[index]
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, index + 1
        shift += 7
    raise VarintError(f"偏移 {offset} 处的 varint 超过 10 字节，文件可能已损坏")


def read_zigzag(data: bytes, offset: int = 0) -> tuple[int, int]:
    """读取 zigzag 编码的 i64。"""
    raw, next_offset = read_u64(data, offset)
    return (raw >> 1) ^ -(raw & 1), next_offset


def try_read_zigzag(data: bytes, offset: int = 0) -> tuple[int, int] | None:
    """增量版 zigzag 读取：数据不足返回 ``None``，超长 varint 仍抛异常。"""
    step = try_read_u64(data, offset)
    if step is None:
        return None
    raw, next_offset = step
    return (raw >> 1) ^ -(raw & 1), next_offset


def to_signed(value: int) -> int:
    """把无符号位模式解释为有符号（用于差值的 wrapping 运算）。"""
    value &= (1 << 64) - 1
    return value - (1 << 64) if value >= (1 << 63) else value
