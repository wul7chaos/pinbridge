"""`pb-replay`：离线读取 `.pbt` 的命令行入口。

输出结构与 Rust 侧 `pinbridge replay` 保持一致（字段名相同）——
两侧的输出能直接对比，是"两个实现没有分叉"最省事的日常检查方式。
"""

from __future__ import annotations

import argparse
import sys
from collections import Counter

from pb import __version__
from pb.trace.event import EventKind
from pb.trace.pbt import PbtReader
from pb.trace.replay import load


def build_parser() -> argparse.ArgumentParser:
    """构造参数解析器。"""
    parser = argparse.ArgumentParser(
        prog="pb-replay",
        description="PinBridge 轨迹离线读取与快速体检",
    )
    parser.add_argument("trace", help=".pbt 轨迹文件")
    parser.add_argument("--head", type=int, default=10, help="打印前 N 条记录（默认 10，0 表示不打印）")
    parser.add_argument("--taint", type=lambda text: int(text, 0), help="从该地址出发做前向污点传播")
    parser.add_argument("--oep-range", help="形如 0x140000000-0x140100000，定位 OEP 候选点")
    parser.add_argument("--version", action="version", version=f"pb-replay {__version__}")
    return parser


def main(argv: list[str] | None = None) -> int:
    """入口。返回进程退出码。"""
    args = build_parser().parse_args(argv)

    with PbtReader(args.trace) as reader:
        records = reader.read_all()
        summary = reader.summary
        meta_text = reader.meta_text
        version = reader.version
        flags = reader.flags
        audit = reader.audit_text

    trace = load(args.trace)

    print(f"文件       : {args.trace}")
    print(f"版本/标志  : v{version} flags=0x{flags:02X}")
    if meta_text.strip():
        print(f"元数据     : {meta_text.strip()}")
    if summary is None:
        print("汇总       : 缺失（轨迹可能被中断，尾部信息不完整）")
    else:
        print(
            f"汇总       : records={summary.records} events={summary.events} "
            f"folded={summary.repeat_folded} dropped={summary.dropped} "
            f"truncated={summary.truncated}"
        )
        print(f"折叠率     : {summary.fold_ratio:.4f}")
        if summary.dropped:
            print("           注意：dropped 非零，说明采集阶段丢过事件，结论必须打折")

    kinds: Counter[str] = Counter(record.kind.tag for record in records)
    print("记录类型   : " + ", ".join(f"{name}×{count}" for name, count in kinds.most_common()))

    if args.head:
        print(f"\n前 {args.head} 条记录：")
        for record in records[: args.head]:
            print(f"  {record}")

    if args.taint is not None:
        tainted = trace.taint_forward([args.taint])
        print(f"\n污点传播（来源 0x{args.taint:X}）：")
        for address, depth in sorted(tainted.items()):
            print(f"  0x{address:X}  depth={depth}")

    if args.oep_range:
        low_text, _, high_text = args.oep_range.partition("-")
        candidate = trace.first_write_then_execute(int(low_text, 0), int(high_text, 0))
        if candidate is None:
            print("\nOEP 候选   : 未找到（区间内没有'写入后被执行'的模式）")
        else:
            print(
                f"\nOEP 候选   : seq={candidate.sequence} address=0x{candidate.address:X}\n"
                f"             依据：{candidate.reason}"
            )

    if audit.strip():
        print("\n审计（前 5 行）：")
        for line in audit.splitlines()[:5]:
            print(f"  {line}")

    # 一句话结论，方便直接贴进报告
    if summary is not None and summary.dropped == 0 and not summary.truncated:
        print("\n结论       : 轨迹完整（无丢弃、未截断），可作为分析依据")
    else:
        print("\n结论       : 轨迹**不完整**，任何结论都必须附带这一前提")
    print(f"（事件类型编号表版本：{len(EventKind)} 项）")
    return 0


if __name__ == "__main__":  # pragma: no cover - 手工调用入口
    sys.exit(main())
