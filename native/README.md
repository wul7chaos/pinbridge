# native/ —— C++ / Pin 层

这一层是 C ABI 的 **C++ 侧**，产物由 Hub 在运行时加载。
规范见 [docs/specs/c-abi.md](../docs/specs/c-abi.md)，头文件是
[include/pinbridge.h](include/pinbridge.h)。

## 两个产物，同一套 ABI

| 产物 | 源文件 | 需要 Pin | 用途 |
|------|--------|----------|------|
| `pinbridge_bridge.dll` | `src/agent_bridge.cpp` | ❌ | 导出 Hub 期望的 8 个 tool 符号，转发给真实 agent DLL |
| `pinbridge.dll` | `src/pinbridge_tool.cpp`（M1） | ✅ | 同一套转发 + Pin 探针、JIT 上下文、TF 虚拟化 |

`src/abi_selfcheck.cpp` 与两个产物都有关：它用 `static_assert` 把跨语言结构体
布局、能力位、事件类型判别值钉在**编译期**。布局漂移应该是编译失败，
而不是运行期把目标进程搞崩。

## 构建

```powershell
pwsh scripts/build-native.ps1            # 配置 + 构建 + ctest
pwsh scripts/build-native.ps1 -RequireAgent   # agent DLL 缺失即失败（CI 用）
```

脚本会自动找 Visual Studio（`vswhere` → `vcvars64.bat`）、Ninja，
以及 CMake（系统没有时用 `uv tool install cmake` 拉一个）。
手工等价命令：

```powershell
cmake -S native -B native/build -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build native/build
ctest --test-dir native/build --output-on-failure
```

## 测试

| CTest | 验证什么 |
|-------|----------|
| `abi_selfcheck` | C 头文件里的布局/位/判别值与预期一致（运行期复查编译期断言） |
| `bridge_roundtrip` | 真实 agent 在场：符号表完整、ABI 一致、**能力闸门跨语言生效**、桩明确返回不支持、3 条事件跨两个语言边界后逐字段完好、参数校验语义正确 |
| `bridge_missing_agent` | agent 缺失时**明确失败**（错误文本给出尝试过的路径），不静默成功 |

Hub 侧还有一条对应的 Rust 集成测试
（[`crates/pinbridge-sys/tests/bridge_end_to_end.rs`](../crates/pinbridge-sys/tests/bridge_end_to_end.rs)）：
它用**真实的 `LoadLibraryA`** 加载 `pinbridge_bridge.dll` 并驱动整条链。
CI 的 `native` job 会设置 `PINBRIDGE_REQUIRE_NATIVE=1`，因此它**不允许跳过**。

## 踩过的坑（别再踩）

| 现象 | 真正原因 | 处理 |
|------|----------|------|
| `error C2001: 字符串字面量中的换行符`，指向一堆无关的行 | 源文件是 UTF-8，MSVC 按系统代码页（936/GBK）解释，连续 UTF-8 字节吞掉了收尾引号 | 加 `/utf-8`（已在 CMakeLists 里对所有 MSVC 构建生效） |
| `ctest` 报 `Unable to find executable: .../Development/x.exe.exe` | `add_test(COMMAND <裸目标名>)` 在部分生成器下解析错误 | 用 `$<TARGET_FILE:...>` |
| 构建日志被上千行"注意: 包含文件:"刷屏，真正的错误淹没其中 | 非英文 MSVC 的 `showIncludes` 前缀与 Ninja 的默认解析不符 | 设 `CMAKE_CL_SHOWINCLUDES_PREFIX` |
| `offsetof` 在头文件里"找不到标识符" | 头文件只包含了 `<stdint.h>` | 补 `<stddef.h>` |

## Intel Pin（M1）

Pin 按 Intel 自己的许可分发，**不入库**（见
[third_party/pin/README.md](../third_party/pin/README.md)）。
启用方式：

```powershell
cmake -S native -B native/build -G Ninja -DPINBRIDGE_WITH_PIN=ON `
      -DPIN_ROOT="<pin>/source/tools/Pin"
```

在此之前，`pinbridge.dll`、`pinbridge_tool.cpp`、`flags_virtualize.cpp`
都不参与构建；需要它们的动作在桥接层与 agent 里都是**显式的 `not_supported`**。
