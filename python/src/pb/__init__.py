"""PinBridge Python 侧。

与 Rust 侧的关系是**同一份规范的两个实现**：

* 读：`.pbt` 轨迹的离线解码、重放、污点传播与反向切片；
* 写：测试与工具链可以直接构造 `.pbt`（例如把外部日志转成轨迹做回归对比）。

两条硬规矩：

1. **与 Rust 实现逐字节一致**。`tests/test_golden_vectors.py` 里的黄金字节
   由两侧共同断言，任何一侧改了编码都会立刻失败。
2. **不做猜测性解析**。遇到未知类型判别值、自相矛盾的折叠标记、越界的变长整数，
   一律抛异常并给出可定位的信息；"尽力而为地解析出一堆看似合理的数据"
   在安全分析里比直接失败危险得多。
"""

from __future__ import annotations

__version__ = "0.1.0"

#: 轨迹格式版本。与 Rust 侧 `pinbridge_trace::pbt::FORMAT_VERSION` 必须相同。
FORMAT_VERSION = 1

#: 文件头 magic。与 Rust 侧 `pinbridge_core::MAGIC` 必须相同。
MAGIC = b"PBT1"

#: 单条事件的参数个数上限。与 Rust 侧 `MAX_ARGS` 必须相同。
MAX_ARGS = 8

#: 协议版本（Hub ↔ CLI/MCP 的 JSON 帧）。与 `pinbridge_core::WIRE_VERSION` 相同。
WIRE_VERSION = 1

__all__ = ["FORMAT_VERSION", "MAGIC", "MAX_ARGS", "WIRE_VERSION", "__version__"]
