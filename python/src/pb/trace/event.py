"""事件模型（Python 侧）。

判别值表必须与 Rust 侧 `pinbridge_trace::event::EventKind` 完全一致：
数值写入文件，**不得重排**，新增只能追加。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import IntEnum

from pb import MAX_ARGS


class EventKind(IntEnum):
    """事件类型。数值与 Rust 侧一一对应。"""

    RESERVED = 0
    BREAKPOINT_HIT = 1
    EXCEPTION = 2
    SYSCALL_ENTER = 3
    SYSCALL_EXIT = 4
    PATTERN_HIT = 5
    MEM_WRITE = 6
    MEM_READ = 7
    REG_WRITE = 8
    TRAP_FLAG = 9
    STATE_CHANGE = 10
    SCRIPT_OUTPUT = 11
    REPEAT = 12

    @property
    def tag(self) -> str:
        """短名（与 Rust `EventKind::name` 相同，出现在 JSON 与 CLI 输出里）。"""
        return _KIND_NAMES[self]

    @classmethod
    def from_code(cls, code: int) -> "EventKind":
        """由判别值还原。

        与 Rust 侧 `EventKind::from_code` 严格对齐：**0 也是非法值**
        （`RESERVED` 只用于构造空事件，出现在文件里说明数据源有问题），
        未知值一律抛 `ValueError`，绝不猜一个默认类型。
        """
        if code == int(cls.RESERVED):
            raise ValueError(
                "事件类型判别值 0 是保留值：文件可能被改写，或写端有 bug"
            )
        try:
            return cls(code)
        except ValueError as error:
            raise ValueError(
                f"未知事件类型判别值 {code}：文件可能来自更新版本，或已被改写"
            ) from error

    @classmethod
    def from_name(cls, name: str) -> "EventKind":
        """由短名还原。"""
        for kind in cls:
            if kind.tag == name:
                return kind
        raise ValueError(f"未知事件类型名 {name!r}")


_KIND_NAMES: dict[EventKind, str] = {
    EventKind.RESERVED: "reserved",
    EventKind.BREAKPOINT_HIT: "breakpoint_hit",
    EventKind.EXCEPTION: "exception",
    EventKind.SYSCALL_ENTER: "syscall_enter",
    EventKind.SYSCALL_EXIT: "syscall_exit",
    EventKind.PATTERN_HIT: "pattern_hit",
    EventKind.MEM_WRITE: "mem_write",
    EventKind.MEM_READ: "mem_read",
    EventKind.REG_WRITE: "reg_write",
    EventKind.TRAP_FLAG: "trap_flag",
    EventKind.STATE_CHANGE: "state_change",
    EventKind.SCRIPT_OUTPUT: "script_output",
    EventKind.REPEAT: "repeat",
}


@dataclass(slots=True)
class Event:
    """一条事件。

    `args` 固定 8 项（与 Rust 的 `[u64; 8]` 对应）：定长让载荷比较与编码都更简单，
    也避免了"不同事件类型参数个数不同"带来的歧义。
    """

    sequence: int
    kind: EventKind
    thread_id: int
    address: int
    args: list[int] = field(default_factory=lambda: [0] * MAX_ARGS)

    def __post_init__(self) -> None:
        if len(self.args) != MAX_ARGS:
            raise ValueError(f"args 必须是 {MAX_ARGS} 项，收到 {len(self.args)}")
        if self.sequence > (1 << 64) - 1:
            raise ValueError("sequence 超出 u64 范围")

    def arg(self, index: int) -> int:
        """读参数（越界返回 0，与 Rust 的 `arg()` 一致）。"""
        return self.args[index] if 0 <= index < MAX_ARGS else 0

    def same_payload(self, other: "Event") -> bool:
        """载荷比较：**不比较序号**。

        这是折叠判定的核心：同一条指令重复执行百万次时，只有序号在变。
        """
        return (
            self.kind == other.kind
            and self.thread_id == other.thread_id
            and self.address == other.address
            and self.args == other.args
        )

    @classmethod
    def repeat_marker(cls, last_sequence: int, folded_count: int, sample: "Event") -> "Event":
        """构造折叠标记。

        * `sequence` = 该重复段最后一条事件的序号；
        * `args[0]` = 被折叠掉的条数（不含第一次出现的那条）；
        * `args[1]` = 原始事件类型判别值。
        """
        args = [0] * MAX_ARGS
        args[0] = folded_count
        args[1] = int(sample.kind)
        return cls(last_sequence, EventKind.REPEAT, sample.thread_id, sample.address, args)

    def __str__(self) -> str:
        rendered = ", ".join(f"0x{value:X}" for value in self.args)
        return (
            f"#{self.sequence} {self.kind.tag} tid={self.thread_id} "
            f"addr=0x{self.address:X} args=[{rendered}]"
        )
