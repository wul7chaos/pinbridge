"""与 Rust 侧的**跨语言一致性**测试。

这是本仓库最重要的一组测试之一：Rust 与 Python 是两个独立实现，
如果它们对同一份 `.pbt` 给出不同结果，那么"轨迹是唯一的真相来源"这句话就不成立。

三层验证：

1. **黄金字节**（`test_pbt.py`）：同一份记录 → 同一条字节流；
2. **常量对齐**：magic / 版本 / 事件类型表 / 脚本哈希，两侧必须完全相同；
3. **端到端**：跑 Rust 的 `pinbridge demo` 生成真实轨迹，用 Python 读回来，
   校验汇总与"记录数 ↔ 展开事件数"的关系。
"""

from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from pb import FORMAT_VERSION, MAGIC, MAX_ARGS, WIRE_VERSION
from pb.trace.event import EventKind
from pb.trace.pbt import FLAG_DELTA_ENCODED, FLAG_REPEAT_FOLDED, FLAG_TRUNCATED, PbtReader
from pb.trace.replay import load

REPO_ROOT = Path(__file__).resolve().parents[2]

#: 与 `crates/pinbridge-hub/src/script.rs` 中 `DEMO_SHA256` 相同的常量。
DEMO_SOURCE_SHA256 = "a98c7127446444ec31a9890062ee7f669d02b955bf07ea513f23f5f3db81671e"
DEMO_SOURCE = "# pinbridge demo hook\nprint('hello')\n"

#: 与 Rust 侧 `EventKind::code()` 一一对应。
EXPECTED_KIND_TABLE = {
    "reserved": 0,
    "breakpoint_hit": 1,
    "exception": 2,
    "syscall_enter": 3,
    "syscall_exit": 4,
    "pattern_hit": 5,
    "mem_write": 6,
    "mem_read": 7,
    "reg_write": 8,
    "trap_flag": 9,
    "state_change": 10,
    "script_output": 11,
    "repeat": 12,
}


def rust_binary() -> Path | None:
    """找到已构建的 pinbridge CLI（找不到就跳过端到端用例）。"""
    for candidate in (
        REPO_ROOT / "target" / "debug" / "pinbridge.exe",
        REPO_ROOT / "target" / "debug" / "pinbridge",
        REPO_ROOT / "target" / "release" / "pinbridge.exe",
        REPO_ROOT / "target" / "release" / "pinbridge",
    ):
        if candidate.exists():
            return candidate
    return None


def test_sha256_matches_the_rust_hub_constant() -> None:
    """脚本哈希跨语言必须一致，否则"AI 加载了哪一版脚本"无法与轨迹对上。"""
    assert hashlib.sha256(DEMO_SOURCE.encode()).hexdigest() == DEMO_SOURCE_SHA256
    # 标准向量："abc"
    assert (
        hashlib.sha256(b"abc").hexdigest()
        == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    )


def test_format_constants_match_rust() -> None:
    assert MAGIC == b"PBT1"
    assert FORMAT_VERSION == 1
    assert MAX_ARGS == 8
    assert WIRE_VERSION == 1
    assert (FLAG_REPEAT_FOLDED, FLAG_DELTA_ENCODED, FLAG_TRUNCATED) == (1, 2, 4)


def test_event_kind_table_matches_rust() -> None:
    actual = {kind.tag: int(kind) for kind in EventKind}
    assert actual == EXPECTED_KIND_TABLE
    for tag, code in EXPECTED_KIND_TABLE.items():
        if code == 0:
            with pytest.raises(ValueError):
                EventKind.from_code(code)
            continue
        assert EventKind.from_code(code).tag == tag
        assert EventKind.from_name(tag).value == code


@pytest.mark.skipif(rust_binary() is None, reason="未找到已构建的 pinbridge CLI")
def test_python_reads_a_trace_written_by_rust(tmp_path: Path) -> None:
    """端到端：Rust 录制 → Python 读取。

    这是唯一能证明"两侧真的在同一个文件格式上会合"的用例——
    单测共享常量只能证明"两边都按我以为的规范写了"，证明不了能互通。
    """
    binary = rust_binary()
    assert binary is not None
    result = subprocess.run(  # noqa: S603 - 路径来自本仓库构建产物
        [str(binary), "demo", "--trace-dir", str(tmp_path)],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    traces = list(tmp_path.glob("*.pbt"))
    assert len(traces) == 1, f"demo 应该产出恰好一份轨迹，实际: {traces}"
    path = traces[0]

    with PbtReader(path) as reader:
        records = reader.read_all()
        summary = reader.summary
        meta_text = reader.meta_text
    assert summary is not None, "Rust 侧正常收尾的轨迹必须带 SUMMARY 段"
    assert summary.records == len(records)
    assert summary.dropped == 0
    assert summary.truncated is False

    meta = json.loads(meta_text)
    assert meta["tool"] == "pinbridge-hub"
    assert meta["session"] == 1
    assert meta["profile"] == "repeat+delta"
    assert meta["flags"] & FLAG_REPEAT_FOLDED

    trace = load(path)
    assert trace.expanded_count() == summary.events, (
        "Python 展开出的事件数必须等于 Rust 记录的 summary.events"
    )
    # demo 里灌入了 4000 条相同的 syscall，折叠率必须很高
    assert summary.fold_ratio > 0.9
    assert any(record.kind == EventKind.REPEAT for record in records)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(pytest.main([__file__, "-v"]))
