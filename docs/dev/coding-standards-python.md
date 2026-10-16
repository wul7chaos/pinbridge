# Python 代码规范

Python 侧只有一个职责：**离线分析 `.pbt`**。它不在目标进程里跑，
所以规范的重点不是性能，而是**可复现**与**不许猜**。

## 工具链

| 工具 | 配置 | 说明 |
|------|------|------|
| 版本 | >= 3.10 | `X | None` 与 `match` 等语法 |
| 依赖 | **运行期零依赖** | 解析轨迹的代码要能在任何机器上直接跑 |
| 格式 | `ruff format`（100 列） | 与 Rust 侧 100 列对齐 |
| Lint | `ruff check` | 选择集见 `python/pyproject.toml` |
| 类型 | `mypy --strict` | 数据格式的字段必须显式 |

```bash
cd python
pip install -e ".[dev]"
ruff check . && ruff format --check . && mypy && pytest -q
```

## 三条硬规矩

### 1. 与 Rust 实现逐字节一致

`.pbt` 的编解码是**同一份规范的两个实现**。
两边共享黄金字节用例（`test_pbt.py` 的 `GOLDEN_EVENT_PAYLOAD_HEX`），
常量由 Rust 测试打印、两侧共同断言。

改编码之前先读 [../specs/pbt-format.md](../specs/pbt-format.md)。
**任何"Python 更方便"的编码都不允许**——格式必须两边都自然。

### 2. 不做猜测性解析

```python
# ❌ 让坏数据看起来能用
kind = EVENT_KINDS.get(code, EventKind.RESERVED)

# ✅ 明确失败，并说清为什么
kind = EventKind.from_code(code)   # 未知值 / 保留值 0 都抛 ValueError
```

同理：

| 输入 | 行为 |
|------|------|
| 未知事件类型 | 抛错（不猜默认值） |
| `argc > 8` | 抛错 |
| 超长 varint（>10 字节） | 抛错（这是损坏，不是"没读完"） |
| 截断的 chunk / 记录 | 抛错，并指出**在第几条记录之后** |
| 自相矛盾的折叠标记 | 严格模式抛错（默认开启） |

**理由**：在安全分析里，一份"看起来完整但实际错误"的报告比一个明确的失败
昂贵得多。

### 3. 区分"没读完"与"坏了"

流式解析里这是最容易出错的地方：

```python
step = try_read_u64(data, cursor)      # 数据不足返回 None（继续读）
if step is None:
    return None                        # 需要更多数据

read_u64(data, cursor)                 # 数据不足抛异常（用于一次性解析）
```

`try_read_u64` 对"超过 10 字节"仍然抛异常——那是损坏，不是"还没读到"。

## 风格约定

### 类型标注

公开函数必须完整标注（`mypy --strict` 会检查）：

```python
def read_u64(data: bytes, offset: int = 0) -> tuple[int, int]: ...
def expand_records(records: Iterable[Event], *, strict: bool = True) -> list[Event]: ...
```

用 `from __future__ import annotations` 统一延迟求值。

### 命名

| 对象 | 风格 | 例 |
|------|------|-----|
| 类 / 数据类型 | PascalCase | `PbtReader` `RepeatFolder` |
| 函数 / 变量 | snake_case | `expand_records` `folded_total` |
| 常量 | UPPER_SNAKE | `CHUNK_EVENT` `MAX_ARGS` |
| 与 Rust 对应的字段名 | **保持一致** | `records` / `events` / `repeat_folded` |

最后一条很重要：字段名两边不同会让"对比两侧输出"这件事变成人工映射，
而人工映射必然出错。

### 文档字符串

* 模块级 docstring 说清**这个模块不做什么**；
* 涉及近似/启发式的函数，必须写明**失效条件**。例：

  ```python
  def taint_forward(self, sources: list[int]) -> dict[int, int]:
      """前向污点传播（结构化近似）。

      近似之处（必须知道，否则会把线索当证据）：
        * 不做字节级偏移追踪——同 4 KiB 页内偏移被当成同一个地址；
        * 不含算术传播（lea rax, [rbx+8] 这类地址计算看不到）；
      """
  ```

* 引用规范时给出文件路径（`docs/specs/pbt-format.md`），不要只说"见文档"。

### 数据结构

* 定长语义用小 `@dataclass(slots=True)`（例如 `Event`、`Summary`）；
* `Event.args` 固定 8 项，`__post_init__` 里校验长度——
  与 Rust 的 `[u64; 8]` 对应；
* **不要**用 `dict` 传递跨语言结构（字段名打错 = 静默变 None）。

## 测试

* 用例与 Rust 侧**一一对应**（同输入、同预期输出），文件头注明对应关系；
* 参数化用 `@pytest.mark.parametrize`，边界值显式列出
  （`0, 1, 0x7F, 0x80, u64::MAX`…）；
* 需要真实轨迹的用例要**能跳过**（找不到构建产物时 `pytest.skip`），
  但**在 CI 里必须真的跑**（CI 会先 `cargo build`）：
  见 `test_rust_parity.py` 的 `test_python_reads_a_trace_written_by_rust`。

**跨语言用例是最有价值的一组**：单测共享常量只能证明"两边都按我以为的规范写了"，
只有"Rust 录 → Python 读"能证明它们真的互通。

## 常见坑

| 坑 | 后果 | 正确做法 |
|----|------|----------|
| Python 的 `>>` 对负数是算术右移，但与定长语义不同 | zigzag 编码错 | 统一在末尾按 64 位掩码截断（`& ((1 << 64) - 1)`） |
| `int` 是任意精度 | 溢出"不会发生"，于是默默产出一个超出 u64 的值 | 写入前显式校验/截断 |
| 直接物化展开后的事件 | 几十亿条事件把内存吃光 | 用 `iter_expanded` 流式处理 |
| 用 `list.append` 在热循环里构造大列表 | 同上 | 生成器 + 调用方决定是否物化 |

## 提交前

```bash
cd python
ruff check . && ruff format --check . && mypy && pytest -q
```

或者直接跑仓库级门禁：`pwsh scripts/verify.ps1`（Windows）/
`bash scripts/verify.sh`（Linux、macOS）。
